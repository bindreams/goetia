// Probe A (temporary): bounded launchctl print calls with random deadlines, through the real path.
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::*;
use crate::manager::budget::Budget;

fn errno_of(e: &cosca::error::Error) -> String {
    match e {
        cosca::error::Error::Io(io) => format!("{:?}", io.raw_os_error()),
        _ => "None".to_string(),
    }
}

#[cfg(target_os = "macos")]
#[skuld::test]
fn probe_bounded_kill_eperm() {
    let n: u32 = std::env::var("PROBE_PASSES").ok().and_then(|v| v.parse().ok()).unwrap_or(20000);
    let mut seed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64 | 1;
    let mut rand = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let (mut exited, mut expired) = (0u32, 0u32);
    let (mut spawn_errs, mut lost_errs) = (0u32, 0u32);
    let (mut spawn_eperm, mut lost_eperm) = (0u32, 0u32);
    let mut eperm_deadlines = Vec::new();
    let started = Instant::now();
    for i in 0..n {
        let ms = 3 + rand() % 12; // a print takes about 11 ms here: centre the deadlines on it
        let mut cmd = command("/bin/launchctl", &["print", "system"]).unwrap();
        let mut reservation = Reservation::for_role(&Role::Query);
        let spawned = match spawn(&mut cmd, Role::Query, &mut reservation) {
            Ok(s) => s,
            Err(e) => {
                spawn_errs += 1;
                let text = format!("{e:?}");
                if text.contains("code: 1,") {
                    spawn_eperm += 1;
                }
                eprintln!("PROBE_SPAWN_ERR pass={i} ms={ms} {text}");
                continue;
            }
        };
        match wait_bounded(spawned, Budget::Bounded(Duration::from_millis(ms)).start()) {
            Ok(Finished::Exited { .. }) => exited += 1,
            Ok(Finished::Expired) => expired += 1,
            Ok(Finished::Left(_)) => unreachable!("a query is never left"),
            Err(lost) => {
                lost_errs += 1;
                let code = errno_of(&lost.error);
                if code == "Some(1)" {
                    lost_eperm += 1;
                    eperm_deadlines.push(ms);
                }
                eprintln!("PROBE_LOST pass={i} ms={ms} errno={code} {:?}", lost.error);
            }
        }
    }
    eprintln!(
        "PROBE_A_RESULT passes={n} secs={} exited={exited} expired={expired} spawn_errs={spawn_errs} \
         spawn_eperm={spawn_eperm} lost_errs={lost_errs} lost_eperm={lost_eperm} eperm_deadlines_ms={eperm_deadlines:?}",
        started.elapsed().as_secs()
    );
}

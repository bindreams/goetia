use crate::util::*;
use std::time::{Duration, Instant};

const LABEL: &str = "com.goetia.probe.timing";

/// Does the "not yet running" window live in `kickstart` (process not
/// spawned yet) or in `launchctl print` (process spawned, launchd's
/// reporting view lags)?
pub fn run_q1(n: usize, full_cycle: bool) {
    hdr(&format!(
        "Q1 {} n={n}",
        if full_cycle { "full cycle (bootout+bootstrap+kickstart), mirrors goetia start()" } else { "kickstart -k only (job stays bootstrapped)" }
    ));

    let path = write_plist(LABEL, &["/bin/sleep", "300"]);
    let target = format!("system/{LABEL}");
    run("launchctl", &["bootout", &target]);

    let mut t_kick = Vec::new();
    let mut t_print_call = Vec::new();
    let mut t_lag = Vec::new();
    let mut t_alive_check = Vec::new();
    let mut attempts_hist: Vec<usize> = Vec::new();

    let mut first_print_running = 0usize;
    let mut first_print_running_with_pid = 0usize;
    let mut first_list_has_pid = 0usize;
    let mut alive_at_kick_return = 0usize;
    let mut kick_gave_pid = 0usize;
    let mut pid_mismatch = 0usize;
    let mut timeouts = 0usize;
    // (kickstart pid alive?, print says running?) -> count
    let mut cross = [[0usize; 2]; 2];
    let mut first_states: std::collections::BTreeMap<String, usize> = Default::default();
    let mut samples_dumped = 0usize;

    if !full_cycle {
        let b = run("launchctl", &["bootstrap", "system", &path]);
        if b.code != 0 {
            println!("bootstrap failed: code={} err={}", b.code, b.err.trim());
            return;
        }
    }

    for i in 0..n {
        if full_cycle {
            run("launchctl", &["bootout", &target]);
            let b = run("launchctl", &["bootstrap", "system", &path]);
            if b.code != 0 {
                println!("iter {i}: bootstrap failed code={} err={}", b.code, b.err.trim());
                continue;
            }
        }

        let kick_args: Vec<&str> = if full_cycle {
            vec!["kickstart", "-p", &target]
        } else {
            vec!["kickstart", "-k", "-p", &target]
        };
        let t0 = Instant::now();
        let k = run("launchctl", &kick_args);
        let t1 = Instant::now();
        t_kick.push(t1 - t0);
        if k.code != 0 {
            println!("iter {i}: kickstart failed code={} out={:?} err={:?}", k.code, k.out, k.err);
            continue;
        }
        let kpid: Option<i32> = k.out.trim().rsplit(|c: char| !c.is_ascii_digit()).find(|s| !s.is_empty()).and_then(|s| s.parse().ok());
        if kpid.is_some() {
            kick_gave_pid += 1;
        }

        // Cheapest possible observation of "did the process actually start":
        // one `kill(pid, 0)` syscall, before any launchctl round-trip.
        let a0 = Instant::now();
        let alive = kpid.map(pid_alive).unwrap_or(false);
        t_alive_check.push(a0.elapsed());
        if alive {
            alive_at_kick_return += 1;
        }

        let p0 = Instant::now();
        let pr = run("launchctl", &["print", &target]);
        let p1 = Instant::now();
        t_print_call.push(p1 - p0);
        let (state, ppid) = if pr.code == 0 { parse_print(&pr.out) } else { (None, None) };
        let running = state.as_deref() == Some("running");
        *first_states.entry(match (pr.code, &state) {
            (0, Some(s)) => s.clone(),
            (0, None) => "<no state field>".into(),
            (c, _) => format!("<print exit {c}>"),
        }).or_insert(0) += 1;
        if running {
            first_print_running += 1;
        }
        if running && ppid.is_some() {
            first_print_running_with_pid += 1;
        }
        cross[alive as usize][running as usize] += 1;
        if let (Some(a), Some(b)) = (kpid, ppid) {
            if a != b {
                pid_mismatch += 1;
            }
        }

        let l = run("launchctl", &["list", LABEL]);
        if l.code == 0 && parse_list_pid(&l.out).is_some() {
            first_list_has_pid += 1;
        }

        if !running && samples_dumped < 5 {
            samples_dumped += 1;
            println!(
                "--- iter {i}: first print did NOT report running. kickstart pid={kpid:?} alive={alive} \
                 print exit={} ---\n{}\n--- list exit={} ---\n{}\n---",
                pr.code,
                pr.out.trim(),
                l.code,
                l.out.trim()
            );
        }

        // How long until `print` does report it? Bounded only because a job
        // that genuinely never starts must not hang the probe.
        let mut attempts = 1usize;
        if running {
            t_lag.push(p1 - t1);
        } else {
            let deadline = t1 + Duration::from_secs(3);
            let mut settled = None;
            while Instant::now() < deadline {
                attempts += 1;
                let r = run("launchctl", &["print", &target]);
                let now = Instant::now();
                if r.code == 0 && parse_print(&r.out).0.as_deref() == Some("running") {
                    settled = Some(now);
                    break;
                }
            }
            match settled {
                Some(s) => t_lag.push(s - t1),
                None => {
                    timeouts += 1;
                    println!("iter {i}: never reported running within 3s");
                }
            }
        }
        attempts_hist.push(attempts);
        // Streamed, not buffered to the end: a step killed by the job
        // timeout must still leave usable data in the log.
        println!(
            "iter {i} kick={:.1}ms kpid={kpid:?} alive={alive} first_print_running={running} attempts={attempts} lag={:.1}ms",
            (t1 - t0).as_secs_f64() * 1e3,
            t_lag.last().map(|d: &Duration| d.as_secs_f64() * 1e3).unwrap_or(f64::NAN)
        );
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    }

    run("launchctl", &["bootout", &target]);
    let _ = std::fs::remove_file(&path);

    println!("\n-- Q1 results ({}) --", if full_cycle { "full cycle" } else { "kickstart -k only" });
    println!("iterations completed: {}", t_kick.len());
    println!("kickstart -p returned a pid: {kick_gave_pid}/{}", t_kick.len());
    println!("that pid was ALIVE the instant kickstart returned: {alive_at_kick_return}/{}", t_kick.len());
    println!(
        "first `launchctl print` after kickstart reported state=running: {first_print_running}/{} ({:.2}% first-try failure)",
        t_kick.len(),
        100.0 * (t_kick.len() as f64 - first_print_running as f64) / t_kick.len().max(1) as f64
    );
    println!("  ...and also carried a pid field: {first_print_running_with_pid}");
    println!("first `launchctl list <label>` carried a PID: {first_list_has_pid}/{}", t_kick.len());
    println!("kickstart pid != print pid: {pid_mismatch}");
    println!("never-running-within-10s: {timeouts}");
    println!("cross-tab [process alive?][print says running?]:");
    println!("  alive=NO  running=NO : {}", cross[0][0]);
    println!("  alive=NO  running=YES: {}", cross[0][1]);
    println!("  alive=YES running=NO : {}   <-- lag is in launchctl print, not in spawn", cross[1][0]);
    println!("  alive=YES running=YES: {}", cross[1][1]);
    println!("first-print `state` values seen: {first_states:?}");
    attempts_hist.sort();
    if !attempts_hist.is_empty() {
        println!(
            "print attempts until running: min={} p50={} max={}",
            attempts_hist[0],
            attempts_hist[attempts_hist.len() / 2],
            attempts_hist[attempts_hist.len() - 1]
        );
    }
    stats("kickstart duration", &mut t_kick);
    stats("one `launchctl print` call", &mut t_print_call);
    stats("one kill(pid,0) syscall", &mut t_alive_check);
    stats("lag: kickstart return -> print reports running", &mut t_lag);
}

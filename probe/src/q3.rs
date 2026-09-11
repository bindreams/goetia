use crate::util::*;
use std::io::{BufRead, BufReader, Write};
use std::mem::zeroed;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

const LABEL: &str = "com.goetia.probe.kq";

pub struct Kq(i32);

impl Kq {
    pub fn new() -> Self {
        Kq(unsafe { libc::kqueue() })
    }

    /// Returns `Ok(())` or the `errno` the registration failed with.
    pub fn watch(&self, pid: i32, fflags: u32) -> Result<(), i32> {
        let mut kev: libc::kevent = unsafe { zeroed() };
        kev.ident = pid as usize;
        kev.filter = libc::EVFILT_PROC;
        kev.flags = libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR;
        kev.fflags = fflags;
        let r = unsafe { libc::kevent(self.0, &kev, 1, null_mut(), 0, null()) };
        if r == -1 {
            Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
        } else {
            Ok(())
        }
    }

    /// Blocks until an event arrives or the bound elapses. Returns `fflags`.
    pub fn wait(&self, bound: Duration) -> Option<u32> {
        let mut out: libc::kevent = unsafe { zeroed() };
        let ts = libc::timespec {
            tv_sec: bound.as_secs() as libc::time_t,
            tv_nsec: bound.subsec_nanos() as libc::c_long,
        };
        let n = unsafe { libc::kevent(self.0, null(), 0, &mut out, 1, &ts) };
        if n > 0 { Some(out.fflags) } else { None }
    }
}

// Hardcoded rather than taken from `libc` so a naming difference cannot
// fail the build on the runner (sys/event.h).
pub const NOTE_EXIT: u32 = 0x8000_0000;
pub const NOTE_FORK: u32 = 0x4000_0000;
pub const NOTE_EXEC: u32 = 0x2000_0000;
pub const NOTE_SIGNAL: u32 = 0x0800_0000;
pub const NOTE_REAP: u32 = 0x1000_0000;
pub const NOTE_TRACK: u32 = 0x0000_0001;
pub const NOTE_TRACKERR: u32 = 0x0000_0002;
pub const NOTE_CHILD: u32 = 0x0000_0004;

pub fn fflag_names(f: u32) -> String {
    let mut v = Vec::new();
    for (bit, name) in [
        (NOTE_EXIT, "NOTE_EXIT"),
        (NOTE_EXEC, "NOTE_EXEC"),
        (NOTE_FORK, "NOTE_FORK"),
        (NOTE_SIGNAL, "NOTE_SIGNAL"),
        (NOTE_REAP, "NOTE_REAP"),
        (NOTE_TRACK, "NOTE_TRACK"),
        (NOTE_TRACKERR, "NOTE_TRACKERR"),
        (NOTE_CHILD, "NOTE_CHILD"),
    ] {
        if f & bit != 0 {
            v.push(name);
        }
    }
    if v.is_empty() { format!("<none: {f:#x}>") } else { v.join("|") }
}

const ALL: u32 = NOTE_EXIT | NOTE_EXEC | NOTE_FORK;

fn errname(e: i32) -> &'static str {
    match e {
        libc::ESRCH => "ESRCH (no such process)",
        libc::EPERM => "EPERM (not permitted)",
        libc::EACCES => "EACCES",
        libc::EINVAL => "EINVAL",
        0 => "ok",
        _ => "other",
    }
}

pub fn run_q3() {
    hdr("Q3: kqueue EVFILT_PROC");
    let euid = unsafe { libc::geteuid() };
    println!("running as euid={euid}");

    // -- 3a. control: a child of ours -------------------------------------
    {
        let mut c = std::process::Command::new("/bin/sleep").arg("30").spawn().expect("spawn");
        let pid = c.id() as i32;
        let kq = Kq::new();
        let reg = kq.watch(pid, ALL);
        println!("3a child-of-ours pid={pid}: register -> {:?} {}", reg, reg.err().map(errname).unwrap_or("ok"));
        let t = Instant::now();
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let ev = kq.wait(Duration::from_secs(5));
        println!("3a after SIGKILL: event={} after {:.0}us", ev.map(fflag_names).unwrap_or("<timeout>".into()), t.elapsed().as_secs_f64() * 1e6);
        let _ = c.wait();
    }

    // -- 3b. a pid that does not exist ------------------------------------
    {
        let kq = Kq::new();
        // A pid far above the wrap point that is essentially certainly free.
        let reg = kq.watch(99998, ALL);
        println!("3b nonexistent pid 99998: register -> {:?} {}", reg, reg.err().map(errname).unwrap_or("ok"));
        println!("    (this is the chicken-and-egg: EVFILT_PROC needs an extant pid, so it cannot be armed *before* the job starts)");
    }

    // -- 3c. a launchd-owned, non-child, root-owned job --------------------
    let path = write_plist(LABEL, &["/bin/sleep", "300"]);
    let target = format!("system/{LABEL}");
    run("launchctl", &["bootout", &target]);
    run("launchctl", &["bootstrap", "system", &path]);
    let k = run("launchctl", &["kickstart", "-p", &target]);
    let jpid: Option<i32> = k.out.trim().rsplit(|c: char| !c.is_ascii_digit()).find(|s| !s.is_empty()).and_then(|s| s.parse().ok());
    println!("3c launchd job pid from kickstart -p: {jpid:?} (raw {:?})", k.out.trim());

    if let Some(pid) = jpid {
        let kq = Kq::new();
        let reg = kq.watch(pid, ALL);
        println!("3c register on launchd-owned non-child pid {pid} as root -> {:?} {}", reg, reg.err().map(errname).unwrap_or("ok"));

        // Does the pid kickstart hands back already point at the *exec'd*
        // program, or at a forked-but-not-yet-exec'd shell of one? If
        // NOTE_EXEC arrives after this point, the exec had not happened
        // when kickstart returned -- and NOTE_EXEC is then the real
        // "it is up" event.
        let t = Instant::now();
        match kq.wait(Duration::from_secs(3)) {
            Some(f) => println!("3c *** an event arrived with no action from us: {} after {:.0}us -- pid was pre-exec at kickstart return", fflag_names(f), t.elapsed().as_secs_f64() * 1e6),
            None => println!("3c no NOTE_EXEC/NOTE_FORK within 3s of kickstart returning -- the pid was already exec'd when kickstart returned"),
        }

        // 3c-2: does NOTE_EXIT fire for this non-child?
        let t = Instant::now();
        run("launchctl", &["kill", "SIGKILL", &target]);
        let ev = kq.wait(Duration::from_secs(5));
        println!("3c after `launchctl kill SIGKILL`: event={} after {:.0}us", ev.map(fflag_names).unwrap_or("<timeout>".into()), t.elapsed().as_secs_f64() * 1e6);
    }

    // -- 3d. the same registration, unelevated ----------------------------
    if euid == 0 {
        let user = std::env::var("PROBE_USER").unwrap_or_else(|_| "runner".into());
        run("launchctl", &["bootout", &target]);
        run("launchctl", &["bootstrap", "system", &path]);
        let k = run("launchctl", &["kickstart", "-p", &target]);
        let jpid: Option<i32> = k.out.trim().rsplit(|c: char| !c.is_ascii_digit()).find(|s| !s.is_empty()).and_then(|s| s.parse().ok());
        match jpid {
            None => println!("3d skipped: no pid to hand the unelevated child"),
            Some(pid) => {
                let exe = std::env::current_exe().expect("exe");
                let mut child = std::process::Command::new("/usr/bin/sudo")
                    .args(["-n", "-u", &user, exe.to_str().unwrap(), "q3-unelevated", &pid.to_string()])
                    .stdout(std::process::Stdio::piped())
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                    .expect("spawn unelevated child");
                let mut stdin = child.stdin.take().unwrap();
                let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
                // Real handshake over a pipe, not a sleep: the child writes
                // READY only once its kevent registration has returned.
                for l in lines.by_ref() {
                    let l = l.unwrap_or_default();
                    println!("  [unelevated] {l}");
                    if l.starts_with("READY") {
                        break;
                    }
                }
                run("launchctl", &["kill", "SIGKILL", &target]);
                let _ = stdin.write_all(b"go\n");
                drop(stdin);
                for l in lines {
                    println!("  [unelevated] {}", l.unwrap_or_default());
                }
                let _ = child.wait();
            }
        }
    }

    run("launchctl", &["bootout", &target]);
    let _ = std::fs::remove_file(&path);
}

/// Child half of 3d, plus 3e (non-child, *same* uid) which only makes
/// sense unelevated.
pub fn run_q3_unelevated(pid: i32) {
    let euid = unsafe { libc::geteuid() };
    println!("euid={euid} (unelevated half)");

    let kq = Kq::new();
    let reg = kq.watch(pid, ALL);
    println!("3d register on root-owned launchd job pid {pid} as euid {euid} -> {:?} {}", reg, reg.err().map(errname).unwrap_or("ok"));
    println!("READY");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    let t = Instant::now();
    let ev = kq.wait(Duration::from_secs(5));
    println!("3d after the root half killed it: event={} after {:.0}us", ev.map(fflag_names).unwrap_or("<timeout>".into()), t.elapsed().as_secs_f64() * 1e6);

    // 3e: non-child, same uid. `sh` forks, prints the pid, and exits, so the
    // sleep is reparented to launchd and is not a child of this process.
    let o = std::process::Command::new("/bin/sh").args(["-c", "/bin/sleep 60 & echo $!"]).output().expect("sh");
    let spid: i32 = String::from_utf8_lossy(&o.stdout).trim().parse().unwrap_or(0);
    let kq2 = Kq::new();
    let reg2 = kq2.watch(spid, ALL);
    println!("3e register on non-child same-uid pid {spid} -> {:?} {}", reg2, reg2.err().map(errname).unwrap_or("ok"));
    let t = Instant::now();
    unsafe { libc::kill(spid, libc::SIGKILL) };
    let ev = kq2.wait(Duration::from_secs(5));
    println!("3e after SIGKILL: event={} after {:.0}us", ev.map(fflag_names).unwrap_or("<timeout>".into()), t.elapsed().as_secs_f64() * 1e6);
    let _ = std::io::stdout().flush();
}

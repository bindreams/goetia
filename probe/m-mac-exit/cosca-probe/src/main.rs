//! Probe for goetia queue questions 3 and 5, against cosca 0.10.0 on macOS.
//!
//! `kill <first|self> <passes>`: what `Child::kill` answers while the child is exiting. `first`: a `sleep` child is sent
//! one kill, then `kill` is called in a tight loop until the child reads as a zombie or gone; `self`: a `true` child
//! exits by itself while `kill` is called in a tight loop. Every call is bracketed by two `sysctl KERN_PROC_PID` reads
//! and tallied with its answer (kind, raw code, source chain) and the debug lines cosca logged during it.
//!
//! `detach <a|b|c|d> <passes>`: what `Child::detach` does with a killed or exited child. a: kill, wait, detach.
//! b: kill, detach. c: kill, wait, drop (what goetia's `wait_bounded` does). d: wait for a natural exit, detach
//! (what goetia's `Reaper` does).
//!
//! Setup mirrors goetia's `bounded::command`: stdin from null, `ContainMode::BestEffort`.

use std::collections::BTreeMap;
use std::error::Error as _;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const P_WEXIT: i32 = 0x2000;
const SRUN: u8 = 2;
const SSLEEP: u8 = 3;
const SZOMB: u8 = 5;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Gone,
    Running,
    Exiting,
    Zombie,
    Other,
    Unreadable,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::Gone => "gone",
            State::Running => "running",
            State::Exiting => "exiting(SRUN+WEXIT)",
            State::Zombie => "zombie(SZOMB)",
            State::Other => "other-state",
            State::Unreadable => "sysctl-error",
        }
    }
}

/// `p_flag` and `p_stat` of `kinfo_proc` by `sysctl KERN_PROC_PID`: offsets 32 and 36 in `extern_proc`
/// (checked against the C probe's `offsetof` in the same job, and by `self_check`).
fn state(pid: u32) -> State {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid as libc::c_int];
    let mut buf = [0u64; 256]; // 2048 bytes, 8-aligned; kinfo_proc is 648
    loop {
        let mut size = std::mem::size_of_val(&buf);
        // SAFETY: `buf` is valid for `size` bytes and `mib` has four entries.
        let r = unsafe { libc::sysctl(mib.as_mut_ptr(), 4, buf.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) };
        if r != 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return State::Unreadable;
        }
        if size == 0 {
            return State::Gone;
        }
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), size) };
        let flag = i32::from_ne_bytes(bytes[32..36].try_into().unwrap());
        let stat = bytes[36];
        return if stat == SZOMB {
            State::Zombie
        } else if flag & P_WEXIT != 0 {
            if stat == SRUN { State::Exiting } else { State::Other }
        } else if stat == SRUN || stat == SSLEEP {
            State::Running
        } else {
            State::Other
        };
    }
}

fn self_check() {
    let me = state(std::process::id());
    assert_eq!(me, State::Running, "offsets of p_flag/p_stat: this process must read as running");
    println!("self check: this process reads {}", me.name());
}

// log capture ---------------------------------------------------------------------------------------------------------

static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Capture;
impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        if record.target().starts_with("cosca") {
            LINES.lock().unwrap().push(format!("{} {}", record.level(), normalize(&record.args().to_string())));
        }
    }
    fn flush(&self) {}
}

fn take_lines() -> Vec<String> {
    std::mem::take(&mut *LINES.lock().unwrap())
}

/// Digit runs become `N`, so a line tallies across pids.
fn normalize(s: &str) -> String {
    let mut out = String::new();
    let mut digits = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            if !digits {
                out.push('N');
            }
            digits = true;
        } else {
            digits = false;
            out.push(c);
        }
    }
    out
}

// tally ---------------------------------------------------------------------------------------------------------------

#[derive(Default)]
struct Tally(BTreeMap<String, u64>);

impl Tally {
    fn add(&mut self, key: String) {
        *self.0.entry(key).or_default() += 1;
    }
    fn dump(&self, title: &str) {
        println!("===== {title} =====");
        for (k, n) in &self.0 {
            println!("{n:8}  {k}");
        }
    }
}

fn describe_error(e: &cosca::error::Error) -> String {
    let mut out = String::new();
    let variant = format!("{e:?}");
    out.push_str(&format!("cosca Error variant `{}`", variant.split(['(', ' ', '{']).next().unwrap_or("?")));
    if let cosca::error::Error::Io(io) = e {
        out.push_str(&format!(", io kind {:?}, raw_os_error {:?}", io.kind(), io.raw_os_error()));
    }
    out.push_str(&format!(", display `{}`", normalize(&e.to_string())));
    let mut depth = 0;
    let mut source = e.source();
    while let Some(s) = source {
        depth += 1;
        let io = s.downcast_ref::<std::io::Error>().map(|io| format!(" [io kind {:?}, raw {:?}]", io.kind(), io.raw_os_error())).unwrap_or_default();
        out.push_str(&format!("; source {depth}: `{}`{io}", normalize(&s.to_string())));
        source = s.source();
    }
    if depth == 0 {
        out.push_str("; no source");
    }
    out
}

fn command(program: &str, args: &[&str]) -> cosca::Command {
    let mut cmd = cosca::run(std::iter::once(program).chain(args.iter().copied()));
    cmd.stdin(cosca::Stdio::null()).expect("stdin from null");
    cmd.contain_with(cosca::ContainMode::BestEffort);
    cmd
}

/// Failure bound for a loop that ends on a state of the child: printed if it is hit.
const BOUND: Duration = Duration::from_secs(10);

// mode kill -----------------------------------------------------------------------------------------------------------

fn run_kill(first: bool, passes: u32) {
    let mut tally = Tally::default();
    let mut windows = 0u64;
    for pass in 0..passes {
        let mut cmd = if first { command("/bin/sleep", &["1000"]) } else { command("/usr/bin/true", &[]) };
        let child = cmd.spawn().expect("spawn");
        let pid = child.pid();
        let t0 = Instant::now();
        let mut call = 0u32;
        loop {
            call += 1;
            take_lines();
            let s0 = state(pid);
            let r = child.kill();
            let s1 = state(pid);
            let lines = take_lines();
            let class = if s0 == s1 { s0.name() } else { "state moved during the call" };
            let answer = match &r {
                Ok(()) => "Ok(())".to_string(),
                Err(e) => format!("Err: {}", describe_error(e)),
            };
            let which = if first && call == 1 { "first kill" } else { "later kill" };
            let mut cosca_lines = lines.iter().map(|l| l.as_str()).collect::<Vec<_>>();
            cosca_lines.dedup();
            tally.add(format!("{which}; child {class}; {answer}; cosca debug: [{}]", cosca_lines.join(" | ")));
            if cosca_lines.iter().any(|l| l.contains("exit has begun")) {
                windows += 1;
            }
            if matches!(s1, State::Zombie | State::Gone) {
                break;
            }
            if t0.elapsed() > BOUND {
                println!("FAILURE BOUND: pass {pass}: the child was neither zombie nor gone after {BOUND:?}");
                break;
            }
        }
        let _ = child.wait();
        drop(child);
    }
    tally.dump(&format!("kill {} passes={passes}; calls whose EPERM cosca read as an exit begun: {windows}", if first { "first" } else { "self" }));
}

// mode detach ---------------------------------------------------------------------------------------------------------

fn run_detach(case: char, passes: u32) {
    let mut tally = Tally::default();
    for _ in 0..passes {
        let mut cmd = match case {
            'd' => command("/bin/sleep", &["0.1"]),
            _ => command("/bin/sleep", &["1000"]),
        };
        let child = cmd.spawn().expect("spawn");
        let pid = child.pid();
        if case != 'd' {
            child.kill().expect("kill");
        }
        if matches!(case, 'a' | 'c' | 'd') {
            child.wait().expect("wait");
        }
        let t0 = Instant::now();
        match case {
            'c' => drop(child),
            _ => {
                if let Err((_child, e)) = child.detach() {
                    panic!("detach refused: {e}");
                }
            }
        }
        let took = t0.elapsed();
        let first = state(pid);
        // The child has exited by now in a, c, d; in b it may still be dying, so wait for the exit to be seen.
        let mut seen = first;
        let t1 = Instant::now();
        while matches!(seen, State::Running | State::Exiting | State::Other) {
            if t1.elapsed() > BOUND {
                println!("FAILURE BOUND: case {case}: the child still ran after {BOUND:?}");
                break;
            }
            seen = state(pid);
        }
        // The observation window is the experiment's own variable: does anything reap it within 1 s?
        let mut later = seen;
        let t2 = Instant::now();
        while later == State::Zombie && t2.elapsed() < Duration::from_secs(1) {
            later = state(pid);
        }
        // A zombie still ours is reaped by us (cleanup, and the evidence of whose it was).
        let mut status = 0;
        // SAFETY: plain waitpid on the pid of our own child.
        let w = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
        let mine = if w == pid as libc::pid_t {
            "waitpid reaped it: it was still our zombie".to_string()
        } else if w == 0 {
            "waitpid: still running".to_string()
        } else {
            format!("waitpid -1, errno {:?}: not ours to reap (gone)", std::io::Error::last_os_error().raw_os_error())
        };
        let fast = if took < Duration::from_millis(50) { "returned in under 50 ms" } else { "returned in 50 ms or more" };
        tally.add(format!(
            "detach {fast}; right after: {}; observed at exit: {}; after the 1 s window: {}; {mine}",
            first.name(),
            seen.name(),
            later.name()
        ));
    }
    let what = match case {
        'a' => "kill, wait, detach",
        'b' => "kill, detach",
        'c' => "kill, wait, drop",
        _ => "natural exit, wait, detach",
    };
    tally.dump(&format!("{what}; passes={passes}"));
}

fn main() {
    log::set_logger(&Capture).expect("logger");
    log::set_max_level(log::LevelFilter::Debug);
    let args: Vec<String> = std::env::args().collect();
    // SAFETY: plain syscall.
    println!("uid {}; cosca 0.10.0", unsafe { libc::getuid() });
    self_check();
    match (args.get(1).map(String::as_str), args.get(2).map(String::as_str), args.get(3).and_then(|s| s.parse::<u32>().ok())) {
        (Some("kill"), Some(v @ ("first" | "self")), Some(n)) => run_kill(v == "first", n),
        (Some("detach"), Some(c @ ("a" | "b" | "c" | "d")), Some(n)) => run_detach(c.chars().next().unwrap(), n),
        _ => {
            eprintln!("usage: see the top of the file");
            std::process::exit(2);
        }
    }
}

// Is a run listed from the moment Task Scheduler launches it? `tsprobe appear <task-path> <actlog> <run|fire> <rounds> <row>`
//
// The action is `tsprobe act rec <actlog> 0 spin_ms=1500`: its first act appends `pid=<n> ct=<creation FILETIME> t=<FILETIME>`
// and it then lives 1.5 s. Two samplers read the task's listing as fast as their calls allow, from before the start
// until the run is over, and keep every sample (time before the call, time after it, what it answered):
//   held   `GetInstances(0)`, `Count`, `get_Item`, `InstanceGuid`, `EnginePID` on a task held across samples
//   fresh  the same after a new `ITaskService` connection, `GetFolder`, `GetTask` for every sample (goetia opens a
//          session per call)
// One clock for everything: `GetSystemTimePreciseAsFileTime`; the process's creation and exit times are the kernel's
// (`GetProcessTimes`, same base).
//   run    per round: `IRegisteredTask::Run` on the idle task; the round is the call, the action's log line, the end of
//          the process (a handle) and an empty listing.
//   fire   per round: the task is a repetition of one-minute ticks. The sampler is recording from 2 s before a minute
//          boundary; the round ends as in `run`. (`rounds` consecutive boundaries.)
// A sample is a VIOLATION when it lies wholly inside [process creation, process exit] and lists no instance. "Listed"
// is: in `run`, an entry with the guid `Run` returned or with the action's pid; in `fire`, any entry.
//
// Output lines (times in ms from the process's creation, negative: before it):
//   A <row> <i> pid=.. first-listed=<t1 of the first sample that listed it> first-listed-t0=<its t0>
//       call=<Run called> ret=<Run returned> last-unlisted-before-first-listed=<t1> listed-before-ct=<n> viol=<n>/<inside>
//       alive=<ct to exit> epid-at-run=<EnginePID Run returned> pid0-listed=<n samples listing the entry with an unreadable pid or 0>
//   V <row> <i> <sampler> <t0> <t1> <answer>      a violation
//   S <row> ... summary
// Every wait is on a log line, a process handle or a listing read again, or is a printed failure bound (120 s).
use crate::util::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use windows::core::*;
use windows::Win32::System::TaskScheduler::*;
use windows::Win32::System::Variant::VARIANT;

#[derive(Clone, Debug)]
struct Entry {
    guid: Option<String>,
    pid: Option<u32>, // None: unreadable
}

#[derive(Clone, Debug)]
struct Sample {
    t0: u64,
    t1: u64,
    kind: &'static str,
    answer: std::result::Result<Vec<Entry>, String>,
}

fn answer_text(a: &std::result::Result<Vec<Entry>, String>) -> String {
    match a {
        Err(e) => format!("ERR {e}"),
        Ok(v) if v.is_empty() => "empty".into(),
        Ok(v) => v
            .iter()
            .map(|e| format!("{}/{}", e.guid.clone().unwrap_or("?".into()), e.pid.map(|p| p.to_string()).unwrap_or("?".into())))
            .collect::<Vec<_>>()
            .join(","),
    }
}

fn read(t: &IRegisteredTask) -> std::result::Result<Vec<Entry>, String> {
    instances_raw(t).map(|items| {
        items
            .into_iter()
            .map(|i| Entry { guid: i.guid.ok(), pid: i.pid.ok() })
            .collect()
    })
}

struct Samplers {
    recording: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    store: Arc<Mutex<Vec<Sample>>>,
    th: Vec<std::thread::JoinHandle<()>>,
}

fn start_samplers(path: &str) -> Samplers {
    let recording = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let store = Arc::new(Mutex::new(Vec::<Sample>::new()));
    let mut th = Vec::new();
    for kind in ["held", "fresh"] {
        let (rec, stp, st, p) = (recording.clone(), stop.clone(), store.clone(), path.to_owned());
        th.push(std::thread::spawn(move || {
            com_init();
            let held = if kind == "held" { task(&p).ok() } else { None };
            while !stp.load(Ordering::Relaxed) {
                let t0 = now_ft();
                let answer = match (&held, kind) {
                    (Some(t), "held") => read(t),
                    _ => task(&p).map_err(|e| format!("task {}", hr(&e))).and_then(|t| read(&t)),
                };
                let t1 = now_ft();
                if rec.load(Ordering::Relaxed) {
                    st.lock().unwrap().push(Sample { t0, t1, kind, answer });
                }
            }
        }));
    }
    Samplers { recording, stop, store, th }
}

fn wait_clock(until: u64) {
    while now_ft() < until {
        std::hint::spin_loop();
    }
}

/// Spins until `cond`; the failure bound is printed, and false is returned, when it is reached first.
fn wait_for(what: &str, bound_ms: f64, mut cond: impl FnMut() -> bool) -> bool {
    let t0 = now_ft();
    loop {
        if cond() {
            return true;
        }
        if ms(t0, now_ft()) > bound_ms {
            println!("B waiting for {what}: failure bound of {bound_ms} ms reached");
            return false;
        }
        std::hint::spin_loop();
    }
}

fn rel(ct: u64, t: u64) -> f64 {
    ms(ct, t)
}

pub fn run(a: &[String]) -> i32 {
    let (path, log, mode, rounds, row) = (&a[0], &a[1], a[2].as_str(), a[3].parse::<usize>().unwrap_or(1), &a[4]);
    com_init();
    let t = task(path).expect("task");
    let s = start_samplers(path);
    let mut viol_rounds = 0usize;
    let mut before_rounds = 0usize;
    let mut first_listed = Vec::new();
    let mut call_to_ct = Vec::new();
    let mut alive_ms = Vec::new();
    let mut failed = 0usize;
    for i in 1..=rounds {
        let _ = std::fs::remove_file(log);
        s.store.lock().unwrap().clear();
        let (mut t_call, mut t_ret) = (0u64, 0u64);
        let mut guid: Option<String> = None;
        let mut epid: String = "-".into();
        match mode {
            "run" => {
                s.recording.store(true, Ordering::SeqCst);
                t_call = now_ft();
                // SAFETY: a plain COM call on a live interface.
                let rt = unsafe { t.Run(&VARIANT::default()) };
                t_ret = now_ft();
                match rt {
                    Ok(rt) => {
                        guid = unsafe { rt.InstanceGuid() }.ok().map(|g| g.to_string());
                        epid = unsafe { rt.EnginePID() }.map(|p| p.to_string()).unwrap_or_else(|e| hr(&e));
                    }
                    Err(e) => {
                        println!("A {row} {i} Run FAILED {}", hr(&e));
                        s.recording.store(false, Ordering::SeqCst);
                        failed += 1;
                        continue;
                    }
                }
            }
            _ => {
                // The next minute boundary at least 4 s away; recording from 2 s before it.
                let now = now_ft();
                let minute = 600_000_000u64; // 60 s in 100 ns ticks
                let b = (now / minute + 1) * minute;
                let b = if b - now < 40_000_000 { b + minute } else { b };
                wait_clock(b - 20_000_000);
                s.recording.store(true, Ordering::SeqCst);
            }
        }
        let got = wait_for("the action's log line", 120_000.0, || !lines(log).is_empty());
        if !got {
            s.recording.store(false, Ordering::SeqCst);
            failed += 1;
            continue;
        }
        let line = lines(log).remove(0);
        let pid: u32 = field(&line, "pid").and_then(|v| v.parse().ok()).unwrap_or(0);
        let ct: u64 = field(&line, "ct").and_then(|v| v.parse().ok()).unwrap_or(0);
        let (mut ct_k, mut exit_k) = (ct, 0u64);
        if let Some(h) = Held::open(pid) {
            h.wait(120_000);
            let (c, e) = h.times();
            ct_k = c;
            exit_k = e;
        }
        let ct = ct_k;
        wait_for("an empty listing", 120_000.0, || {
            matches!(read(&t), Ok(v) if v.is_empty())
        });
        s.recording.store(false, Ordering::SeqCst);
        let samples = s.store.lock().unwrap().clone();

        let listed = |sm: &Sample| -> bool {
            match &sm.answer {
                Err(_) => false,
                Ok(v) => {
                    if mode == "run" {
                        v.iter().any(|e| e.pid == Some(pid) || e.guid.as_deref().zip(guid.as_deref()).is_some_and(|(x, y)| same_guid(x, y)))
                    } else {
                        !v.is_empty()
                    }
                }
            }
        };
        let mut first: Option<&Sample> = None;
        let mut before_ct = 0usize;
        let mut inside = 0usize;
        let mut viol = 0usize;
        let mut pid0 = 0usize;
        for sm in &samples {
            if listed(sm) {
                if first.map_or(true, |f| sm.t1 < f.t1) {
                    first = Some(sm);
                }
                if sm.t1 <= ct {
                    before_ct += 1;
                }
                if let Ok(v) = &sm.answer {
                    if v.iter().any(|e| e.pid.map_or(true, |p| p == 0)) {
                        pid0 += 1;
                    }
                }
            }
            if exit_k != 0 && sm.t0 >= ct && sm.t1 <= exit_k && !listed(sm) {
                viol += 1;
                println!("V {row} {i} {} {:.3} {:.3} {}", sm.kind, rel(ct, sm.t0), rel(ct, sm.t1), answer_text(&sm.answer));
            }
            if exit_k != 0 && sm.t0 >= ct && sm.t1 <= exit_k {
                inside += 1;
            }
        }
        let first_t1 = first.map(|f| rel(ct, f.t1));
        let first_t0 = first.map(|f| rel(ct, f.t0));
        // The last sample that began after Run was called (or the recording began) and did not list it, before the first that did.
        let last_unlisted = samples
            .iter()
            .filter(|sm| !listed(sm) && first.map_or(true, |f| sm.t1 <= f.t1))
            .map(|sm| rel(ct, sm.t1))
            .fold(f64::NEG_INFINITY, f64::max);
        println!(
            "A {row} {i} pid={pid} first-listed={} first-listed-t0={} call={} ret={} last-unlisted-before-first={} listed-before-ct={before_ct} viol={viol}/{inside} alive={:.1} epid-at-run={epid} pid0-listed={pid0} samples={}",
            first_t1.map(|v| format!("{v:.3}")).unwrap_or("NEVER".into()),
            first_t0.map(|v| format!("{v:.3}")).unwrap_or("NEVER".into()),
            if t_call == 0 { "-".into() } else { format!("{:.3}", rel(ct, t_call)) },
            if t_ret == 0 { "-".into() } else { format!("{:.3}", rel(ct, t_ret)) },
            if last_unlisted.is_finite() { format!("{last_unlisted:.3}") } else { "-".into() },
            rel(ct, exit_k),
            samples.len()
        );
        if viol > 0 {
            viol_rounds += 1;
        }
        if before_ct > 0 {
            before_rounds += 1;
        }
        if let Some(v) = first_t1 {
            first_listed.push(v);
        }
        if t_call != 0 {
            call_to_ct.push(-rel(ct, t_call));
        }
        alive_ms.push(rel(ct, exit_k));
    }
    s.stop.store(true, Ordering::SeqCst);
    for h in s.th {
        let _ = h.join();
    }
    println!(
        "S {row} mode={mode} rounds={rounds} failed={failed} rounds-with-violation={viol_rounds} rounds-listed-before-process-creation={before_rounds}"
    );
    println!("S {row} first-listed t1 minus ct (ms): {}", stats(first_listed));
    println!("S {row} Run called before ct (ms): {}", stats(call_to_ct));
    println!("S {row} ct to exit (ms): {}", stats(alive_ms));
    0
}

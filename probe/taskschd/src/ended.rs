// L1-L3. `ended <task> <exp> <runs> <label> <actlog> [churn]`
//
// Per run: Run the task, then sample the listing in a tight loop until nothing is listed. A sample reads the listing
// step by step (GetInstances, Count, get_Item, InstanceGuid, EnginePID: each can fail on its own), and classifies our
// instance:
//   alive                       the entry reads, its pid opens, the process has not exited
//   exited                      the entry reads, its pid opens, the process object is signalled
//   open-fail-<n>               the entry reads, OpenProcess(pid) fails with Win32 error n (87: no such process)
//   pid0                        the entry reads with EnginePID 0
//   listed-unreadable+<p>       the entry is listed but a read of it fails (see the tally); <p> is the state of the
//                               process Run returned: alive, exited, gone (no such pid), open-fail
//   empty                       nothing is listed
// The first sample in which the process is gone but the entry is still listed (exited, open-fail-87,
// listed-unreadable+process-exited/gone) is the "ended-but-listed" window; the experiment `exp` acts there once:
//   l1         nothing (pure sampling)
//   l2run      IRegisteredTask::Run
//   l2runex0   RunEx(flags 0)
//   l2runex2   RunEx(TASK_RUN_IGNORE_CONSTRAINTS)
//   l3stop     IRunningTask State / Refresh / Stop / State
//   l3taskstop IRegisteredTask::Stop(0)
//   fire       nothing at the hook; the action itself emits an event that an EventTrigger turns into a second start
//
// The action (`tsprobe act rec <actlog> ...`) appends `pid=<n> ct=<creation FILETIME> t=<FILETIME>` as its first act:
// that is the ground truth for "the same process".
use crate::util::*;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use windows::core::*;
use windows::Win32::System::TaskScheduler::*;
use windows::Win32::System::Variant::VARIANT;

#[derive(Default)]
struct Agg {
    runs: usize,
    tally: BTreeMap<String, u64>,
    shapes: BTreeMap<String, u64>,
    samples: u64,
    alive: u64,
    exited: u64,
    pid0_samples: u64,
    pid0_runs: usize,
    pid0_ms: Vec<f64>,
    window_runs: usize,
    never_opened_runs: usize,
    linger_seen_ms: Vec<f64>,
    linger_exit_ms: Vec<f64>,
    unreadable_ms: Vec<f64>,
    first_bad_ms: Vec<f64>,
    reuse_ms: Vec<f64>,
    starts: BTreeMap<usize, u64>,
}

impl Agg {
    fn t(&mut self, k: impl Into<String>) {
        *self.tally.entry(k.into()).or_default() += 1;
    }
}

struct Churn {
    log: Arc<Mutex<Vec<(u32, u64, u64)>>>, // pid, creation time, our clock at creation
    stop: Arc<AtomicBool>,
    th: Vec<std::thread::JoinHandle<()>>,
}

/// Three threads creating and reaping tiny processes, so pids are freed and handed out again as fast as they can be.
fn start_churn() -> Churn {
    let log = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let exe = std::env::current_exe().unwrap();
    let th = (0..3)
        .map(|_| {
            let (l2, s2, exe) = (log.clone(), stop.clone(), exe.clone());
            std::thread::spawn(move || {
                while !s2.load(Ordering::Relaxed) {
                    let t0 = now_ft();
                    let Ok(mut c) = std::process::Command::new(&exe)
                        .args(["act", "noop"])
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                    else {
                        continue;
                    };
                    let pid = c.id();
                    let ct = probe_pid(pid).map(|p| p.ct).unwrap_or(0);
                    l2.lock().unwrap().push((pid, ct, t0));
                    let _ = c.wait();
                }
            })
        })
        .collect();
    Churn { log, stop, th }
}

fn lr(t: &IRegisteredTask) -> String {
    // SAFETY: plain COM calls.
    unsafe {
        format!(
            "last_result={} last_run={}",
            t.LastTaskResult().map(|v| format!("0x{:08X}", v as u32)).unwrap_or_else(|e| format!("err {}", hr(&e))),
            t.LastRunTime().map(ole_date).unwrap_or_else(|e| format!("err {}", hr(&e)))
        )
    }
}

fn r2s<T: std::fmt::Debug>(r: Result<T>) -> String {
    match r {
        Ok(v) => format!("ok({v:?})"),
        Err(e) => format!("err {}", hr(&e)),
    }
}

struct Call {
    a: u64,
    b: u64,
    pid: u32,
    guid: String,
    err: Option<String>,
}

/// `l2spam*`: calls Run / RunEx in a tight loop from its own thread from the first run's start until a call returns an
/// instance with a pid (a start), so some calls land in the ended-but-listed window; each call is timed.
fn spam(path: String, kind: u8, stop: Arc<AtomicBool>) -> Vec<Call> {
    com_init();
    let t = task(&path).expect("task in the spam thread");
    let mut v = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let a = now_ft();
        // SAFETY: plain COM calls.
        let r = unsafe {
            match kind {
                0 => t.Run(&VARIANT::default()),
                1 => t.RunEx(&VARIANT::default(), 0, 0, &BSTR::new()),
                _ => t.RunEx(&VARIANT::default(), 2, 0, &BSTR::new()),
            }
        };
        let b = now_ft();
        match r {
            Ok(rt) => {
                let pid = unsafe { rt.EnginePID() }.unwrap_or(0);
                let guid = unsafe { rt.InstanceGuid() }.map(|x| x.to_string()).unwrap_or_default();
                v.push(Call { a, b, pid, guid, err: None });
                if pid != 0 {
                    break;
                }
            }
            Err(e) => {
                v.push(Call { a, b, pid: 0, guid: String::new(), err: Some(hr(&e)) });
                break;
            }
        }
    }
    v
}

/// What the experiment does in the window. Returns a note and the second instance's guid if one was returned.
fn hook(exp: &str, _path: &str, t: &IRegisteredTask, it: Option<&IRunningTask>, agg: &mut Agg) -> (String, Option<String>) {
    // The experiment's own call is made first, with nothing between the sample and the call.
    // SAFETY: plain COM calls.
    unsafe {
        let early: Option<Result<IRunningTask>> = match exp {
            "l2run" => Some(t.Run(&VARIANT::default())),
            "l2runex0" => Some(t.RunEx(&VARIANT::default(), 0, 0, &BSTR::new())),
            "l2runex2" => Some(t.RunEx(&VARIANT::default(), 2, 0, &BSTR::new())),
            _ => None,
        };
        let before = lr(t);
        match exp {
            "l2run" | "l2runex0" | "l2runex2" => {
                let r = early.unwrap();
                match r {
                    Ok(rt) => {
                        let g = rt.InstanceGuid().map(|b| b.to_string()).unwrap_or_default();
                        let pid = rt.EnginePID().unwrap_or(0);
                        let st = r2s(rt.State().map(|s| s.0));
                        agg.t(format!("{exp}: call ok, returned pid {}", if pid == 0 { "0" } else { "nonzero" }));
                        (format!("hook[{exp} ok guid={g} pid={pid} state={st}] at-hook[{before}]"), Some(g))
                    }
                    Err(e) => {
                        agg.t(format!("{exp}: call err {}", hr(&e)));
                        (format!("hook[{exp} err {}] at-hook[{before}]", hr(&e)), None)
                    }
                }
            }
            "l3stop" => {
                let Some(it) = it else {
                    agg.t("l3: no entry object to call Stop on");
                    return (format!("hook[no entry object] at-hook[{before}]"), None);
                };
                let s0 = r2s(it.State().map(|s| s.0));
                let rf = r2s(it.Refresh());
                let s1 = r2s(it.State().map(|s| s.0));
                let st = r2s(it.Stop());
                let s2 = r2s(it.State().map(|s| s.0));
                let rf2 = r2s(it.Refresh());
                let s3 = r2s(it.State().map(|s| s.0));
                agg.t(format!("l3 state-before-refresh {s0}"));
                agg.t(format!("l3 refresh {rf}"));
                agg.t(format!("l3 state-after-refresh {s1}"));
                agg.t(format!("l3 Stop {st}"));
                agg.t(format!("l3 state-after-stop {s2}"));
                agg.t(format!("l3 refresh-after-stop {rf2}"));
                agg.t(format!("l3 state-after-stop-refresh {s3}"));
                (format!("hook[state {s0} refresh {rf} state {s1} Stop {st} state {s2} refresh {rf2} state {s3}] at-hook[{before}]"), None)
            }
            "l3taskstop" => {
                let st = r2s(t.Stop(0));
                agg.t(format!("l3 IRegisteredTask::Stop {st}"));
                (format!("hook[task Stop {st}] at-hook[{before}]"), None)
            }
            _ => (format!("at-hook[{before}]"), None),
        }
    }
}

pub fn run(a: &[String]) -> i32 {
    com_init();
    let path = a[0].as_str();
    let exp = a[1].as_str();
    let runs: usize = a[2].parse().unwrap();
    let label = a[3].as_str();
    let actlog = a[4].as_str();
    let churn = a.get(5).map(|s| s == "churn").unwrap_or(false);
    let settle_ms: f64 = if exp == "fire" { 1500.0 } else { 0.0 };
    let l2 = exp.starts_with("l2") || exp == "fire";
    let spam_kind: Option<u8> = match exp {
        "l2spam" => Some(0),
        "l2spamex0" => Some(1),
        "l2spamex2" => Some(2),
        _ => None,
    };
    let t = task(path).expect("task");
    let mut agg = Agg::default();
    let ch = if churn { Some(start_churn()) } else { None };
    println!("##### ended label={label} exp={exp} runs={runs} churn={churn} task={path}");

    for i in 1..=runs {
        let n_before = lines(actlog).len();
        if exp == "fire" {
            // The action deletes this file and, only if it could, writes the event that the trigger turns into a start.
            let _ = std::fs::write(format!("{actlog}.token"), b"x");
        }
        let t_run = now_ft();
        // SAFETY: plain COM calls throughout.
        let rt = unsafe { t.Run(&VARIANT::default()) };
        let rt = match rt {
            Ok(r) => r,
            Err(e) => {
                agg.t(format!("Run failed {}", hr(&e)));
                println!("run {i}: Run failed {}", hr(&e));
                continue;
            }
        };
        agg.runs += 1;
        let t_ret = now_ft();
        let guid = unsafe { rt.InstanceGuid() }.map(|b| b.to_string()).unwrap_or_default();
        let run_pid = unsafe { rt.EnginePID() }.unwrap_or(0);
        let spam_stop = Arc::new(AtomicBool::new(false));
        let spam_h = spam_kind.map(|k| {
            let (p, s) = (path.to_string(), spam_stop.clone());
            std::thread::spawn(move || spam(p, k, s))
        });

        let mut obs: Vec<(u32, u64)> = Vec::new();
        let (mut samples, mut alive, mut exited) = (0u64, 0u64, 0u64);
        let (mut pid0_first, mut pid0_last): (Option<u64>, u64) = (None, 0);
        let (mut first_pid, mut pid_changed) = (None::<u32>, false);
        let (mut opened_ok, mut hooked) = (false, false);
        let (mut exit_seen, mut exit_ft): (Option<u64>, Option<u64>) = (None, None);
        let (mut bad_first, mut bad_last): (Option<u64>, u64) = (None, 0);
        let mut drop1: Option<u64> = None;
        let mut note = String::new();
        let mut guid2: Option<String> = None;
        let mut seen2 = false;
        let mut consec_err = 0u32;
        let mut timeline: Vec<(f64, String)> = Vec::new();
        let mut last_short = String::new();
        let mut shape: Vec<String> = Vec::new();
        let mut codes = BTreeMap::<u32, u32>::new();
        let mut fail_kinds = BTreeMap::<String, u32>::new();

        loop {
            let raw = instances_raw(&t);
            let ts = now_ft();
            let mut short = String::new();
            let mut detail = String::new();
            let mut ended = false;
            let mut mine_it: Option<IRunningTask> = None;
            let mut is_mine = false;
            match &raw {
                Err(e) => {
                    short = "listing-error".into();
                    detail = e.clone();
                    agg.t(format!("listing error {e}"));
                    consec_err += 1;
                    if consec_err > 100_000 {
                        break;
                    }
                }
                Ok(v) => {
                    consec_err = 0;
                    if let Some(g2) = &guid2 {
                        if v.iter().any(|x| x.guid.as_ref().map_or(false, |g| same_guid(g, g2))) {
                            seen2 = true;
                        }
                    }
                    if v.is_empty() {
                        short = "empty".into();
                    } else {
                        let m = v
                            .iter()
                            .find(|x| x.guid.as_ref().map_or(false, |g| same_guid(g, &guid)))
                            .or_else(|| if v.len() == 1 && guid2.is_none() && v[0].guid.is_err() { Some(&v[0]) } else { None });
                        match m {
                            None => short = "other-instance-only".into(),
                            Some(m) => {
                                is_mine = true;
                                mine_it = m.it.clone();
                                samples += 1;
                                if let Err(e) = &m.guid {
                                    *fail_kinds.entry(format!("InstanceGuid unreadable: {e}")).or_default() += 1;
                                }
                                match &m.pid {
                                    Err(e) => {
                                        let pr = if run_pid != 0 { probe_pid(run_pid) } else { Err(0) };
                                        let (sh, en) = match pr {
                                            Ok(p) if p.exited => {
                                                exit_ft.get_or_insert(p.exit_ft);
                                                ("listed-unreadable+process-exited", true)
                                            }
                                            Ok(_) => ("listed-unreadable+process-alive", false),
                                            Err(87) => ("listed-unreadable+process-gone", true),
                                            Err(_) => ("listed-unreadable+open-fail", false),
                                        };
                                        short = sh.into();
                                        ended = en;
                                        detail = e.clone();
                                        *fail_kinds.entry(format!("entry read failed: {e} ({sh})")).or_default() += 1;
                                    }
                                    Ok(0) => {
                                        short = "pid0".into();
                                        agg.pid0_samples += 1;
                                        pid0_first.get_or_insert(ts);
                                        pid0_last = ts;
                                    }
                                    Ok(p) => {
                                        match first_pid {
                                            None => first_pid = Some(*p),
                                            Some(q) if q != *p => pid_changed = true,
                                            _ => {}
                                        }
                                        match probe_pid(*p) {
                                            Ok(pi) => {
                                                opened_ok = true;
                                                if !obs.contains(&(*p, pi.ct)) {
                                                    obs.push((*p, pi.ct));
                                                }
                                                if pi.exited {
                                                    short = "exited".into();
                                                    ended = true;
                                                    exited += 1;
                                                    exit_ft.get_or_insert(pi.exit_ft);
                                                    *codes.entry(pi.code).or_default() += 1;
                                                } else {
                                                    short = "alive".into();
                                                    alive += 1;
                                                }
                                            }
                                            Err(e) => {
                                                short = format!("open-fail-{e}");
                                                ended = e == 87;
                                                *fail_kinds
                                                    .entry(format!("listed pid {} OpenProcess failed win32={e} ({})", "readable", if opened_ok { "after an earlier successful open in this run" } else { "never opened in this run" }))
                                                    .or_default() += 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if !is_mine && drop1.is_none() && short != "listing-error" {
                drop1 = Some(ts);
            }
            if short != last_short {
                timeline.push((ms(t_ret, ts), format!("{short}{}", if detail.is_empty() { String::new() } else { format!("[{detail}]") })));
                if shape.last().map_or(true, |s| *s != short) {
                    shape.push(short.clone());
                }
                last_short = short.clone();
            }
            if ended || short.starts_with("listed-unreadable") {
                bad_first.get_or_insert(ts);
                bad_last = ts;
            }
            if ended {
                exit_seen.get_or_insert(ts);
                if !hooked {
                    hooked = true;
                    let (n, g) = hook(exp, path, &t, mine_it.as_ref(), &mut agg);
                    note = n;
                    guid2 = g;
                }
            }
            if short == "empty" {
                if settle_ms > 0.0 && ms(drop1.unwrap_or(ts), ts) < settle_ms {
                    continue;
                }
                break;
            }
            if short == "other-instance-only" && !l2 {
                break;
            }
        }
        spam_stop.store(true, Ordering::Relaxed);
        let calls = spam_h.map(|h| h.join().unwrap_or_default()).unwrap_or_default();
        let drop1 = drop1.unwrap_or_else(now_ft);
        let after = lr(&t);
        if exp == "l1" && i <= 3 {
            // Nothing is listed now.
            let r = unsafe { t.Stop(0) };
            agg.t(format!("IRegisteredTask::Stop(0) with nothing running: {}", r2s(r)));
        }
        let mut spam_note = String::new();
        if !calls.is_empty() {
            for c in &calls {
                let phase = if c.a >= drop1 {
                    "after the listing dropped"
                } else if bad_first.map_or(false, |u| c.a >= u) {
                    "in the ended-but-listed window"
                } else {
                    "before the window (the first instance alive)"
                };
                let out = if c.err.is_some() { "error" } else if c.pid != 0 { "started an instance with a pid" } else { "returned an instance with no pid (ignored)" };
                agg.t(format!("Run-call issued {phase}: {out}"));
                if let Some(e) = &c.err {
                    agg.t(format!("Run-call error {e}"));
                }
            }
            let u = bad_first.unwrap_or(drop1);
            let last3: Vec<String> = calls
                .iter()
                .rev()
                .take(3)
                .rev()
                .map(|c| format!("[a-U={:+.2} b-U={:+.2} b-E={:+.2} pid={} guid={}]", ms(u, c.a), ms(u, c.b), ms(drop1, c.b), c.pid, c.guid))
                .collect();
            spam_note = format!("spam: {} calls; last three (U = first ended/unreadable sample, E = the listing's drop): {}", calls.len(), last3.join(" "));
        }

        // What the action itself recorded.
        let all = lines(actlog);
        let new: Vec<&String> = all.iter().skip(n_before).collect();
        *agg.starts.entry(new.len()).or_default() += 1;
        let rec_pid: u32 = new.first().and_then(|l| field(l, "pid")).and_then(|v| v.parse().ok()).unwrap_or(0);
        let rec_ct: u64 = new.first().and_then(|l| field(l, "ct")).and_then(|v| v.parse().ok()).unwrap_or(0);
        if new.is_empty() {
            agg.t("action left no record");
        }
        let mut mism = Vec::new();
        for (p, c) in &obs {
            if rec_pid != 0 && *p != rec_pid {
                mism.push(format!("listed pid {p} is not the action's pid {rec_pid}"));
                agg.t("IDENTITY: the listed pid differs from the action's own pid");
            }
            if rec_ct != 0 && *c != rec_ct {
                mism.push(format!("pid {p} opened with creation {c}, action's own is {rec_ct}"));
                agg.t("IDENTITY: the opened process has another creation time than the action");
            }
        }
        if run_pid != 0 && rec_pid != 0 && run_pid != rec_pid {
            agg.t("IDENTITY: Run's EnginePID differs from the action's own pid");
            mism.push(format!("Run returned pid {run_pid}, action is {rec_pid}"));
        }
        if pid_changed {
            agg.t("the listed pid changed during the run");
        }
        let mut coll = String::new();
        if let Some(ch) = &ch {
            // The first churn process created with the action's pid after this run began: how long after the drop?
            let first = ch.log.lock().unwrap().iter().filter(|(p, _, tc)| *p == rec_pid && *tc >= t_run).map(|(_, _, tc)| *tc).min();
            if let Some(tc) = first {
                agg.reuse_ms.push(ms(drop1, tc));
            }
            let hit: Vec<_> = ch.log.lock().unwrap().iter().filter(|(p, _, tc)| *p == rec_pid && *tc >= t_run && *tc <= drop1).cloned().collect();
            if !hit.is_empty() {
                agg.t("CHURN: a new process got the action's pid before the listing dropped");
                coll = format!("churn pid collision before drop: {hit:?}");
            }
        }
        if !opened_ok && first_pid.is_some() {
            agg.never_opened_runs += 1;
        }
        if run_pid == 0 {
            agg.t("Run returned EnginePID 0");
        }
        let linger_seen = exit_seen.map(|e| ms(e, drop1));
        let linger_exit = exit_ft.filter(|_| exit_seen.is_some()).map(|e| ms(e, drop1));
        if let Some(v) = linger_seen {
            agg.window_runs += 1;
            agg.linger_seen_ms.push(v);
        }
        if let Some(v) = linger_exit {
            agg.linger_exit_ms.push(v);
        }
        if let Some(f) = bad_first {
            agg.unreadable_ms.push(ms(f, bad_last.max(f)));
            agg.first_bad_ms.push(ms(t_ret, f));
        }
        if let Some(f) = pid0_first {
            agg.pid0_runs += 1;
            agg.pid0_ms.push(ms(f, pid0_last));
        }
        agg.samples += samples;
        agg.alive += alive;
        agg.exited += exited;
        for (k, v) in &codes {
            agg.t(format!("exit code seen in an exited sample: 0x{k:08X} (x{v})"));
        }
        for (k, v) in &fail_kinds {
            agg.t(format!("{k} (x{v} samples)"));
        }
        *agg.shapes.entry(shape.join(" > ")).or_default() += 1;
        let second = if new.len() >= 2 {
            let t2: u64 = field(new[1], "t").and_then(|v| v.parse().ok()).unwrap_or(0);
            format!("second start {:+.2} ms from the listing's drop", ms(drop1, t2))
        } else {
            String::new()
        };
        let tl = timeline.iter().map(|(t, s)| format!("+{t:.1}ms {s}")).collect::<Vec<_>>().join(" | ");
        println!(
            "run {i}: guid={guid} run_pid={run_pid} rec_pid={rec_pid} run_ret_ms={:.1} samples={samples} alive={alive} exited={exited} \
             window_ms={} exit_to_drop_ms={} starts={} second2_listed={seen2} {second} {note} after[{after}] {} {coll} {spam_note} timeline: {tl}",
            ms(t_run, t_ret),
            linger_seen.map(|v| format!("{v:.2}")).unwrap_or("-".into()),
            linger_exit.map(|v| format!("{v:.2}")).unwrap_or("-".into()),
            new.len(),
            mism.join("; ")
        );
    }

    if let Some(mut c) = ch {
        c.stop.store(true, Ordering::Relaxed);
        for th in c.th.drain(..) {
            let _ = th.join();
        }
        let n = c.log.lock().unwrap().len();
        println!("churn processes created: {n}");
    }
    println!("===== SUMMARY label={label} exp={exp} runs_ok={} churn={churn}", agg.runs);
    println!("samples of our entry={} alive={} exited={} listed-pid0-samples={}", agg.samples, agg.alive, agg.exited, agg.pid0_samples);
    println!("runs with an ended-but-listed window (process gone, entry still listed): {}", agg.window_runs);
    println!("runs where the listed pid was never openable: {}", agg.never_opened_runs);
    println!("window_ms (first ended sample to the drop of the listing): {}", stats(agg.linger_seen_ms.clone()));
    println!("exit_to_drop_ms (the process's ExitTime to the sample that found the entry gone): {}", stats(agg.linger_exit_ms.clone()));
    println!("first bad sample (ended or unreadable) ms after Run returned: {}", stats(agg.first_bad_ms.clone()));
    println!("bad (ended or unreadable) duration ms: {}", stats(agg.unreadable_ms.clone()));
    println!("pid0 runs={} duration_ms: {}", agg.pid0_runs, stats(agg.pid0_ms.clone()));
    if churn {
        println!(
            "the action's pid was handed to a churn process in {} of {} runs; the start of that CreateProcess relative to the listing's drop, ms (negative = before the drop): {}",
            agg.reuse_ms.len(),
            agg.runs,
            stats(agg.reuse_ms.clone())
        );
    }
    println!("action starts per run: {:?}", agg.starts);
    for (k, v) in &agg.shapes {
        println!("shape of the listing over a run: {k}: {v} runs");
    }
    for (k, v) in &agg.tally {
        println!("tally: {k}: {v}");
    }
    0
}

/// An action that cannot start: what is listed, for how long, what `Run` and `LastTaskResult` say.
pub fn nostart(a: &[String]) -> i32 {
    com_init();
    let path = a[0].as_str();
    let runs: usize = a[1].parse().unwrap();
    let t = task(path).expect("task");
    let mut tally = BTreeMap::<String, u32>::new();
    let mut listed_ms = Vec::new();
    println!("##### nostart task={path} runs={runs}");
    for i in 1..=runs {
        // SAFETY: plain COM calls.
        let rt = unsafe { t.Run(&VARIANT::default()) };
        let rt = match rt {
            Ok(r) => r,
            Err(e) => {
                *tally.entry(format!("Run failed {}", hr(&e))).or_default() += 1;
                println!("run {i}: Run failed {} {}", hr(&e), lr(&t));
                continue;
            }
        };
        let guid = unsafe { rt.InstanceGuid() }.map(|b| b.to_string()).unwrap_or_default();
        let run_pid = unsafe { rt.EnginePID() }.unwrap_or(0);
        let t0 = now_ft();
        let (mut ever, mut last) = (false, t0);
        let mut pids = BTreeMap::<String, u32>::new();
        loop {
            let Ok(list) = instances_raw(&t) else { continue };
            let ts = now_ft();
            if list.is_empty() {
                break;
            }
            for m in &list {
                if m.guid.as_ref().map_or(true, |g| same_guid(g, &guid)) {
                    ever = true;
                    last = ts;
                    *pids.entry(format!("{:?}", m.pid)).or_default() += 1;
                }
            }
        }
        *tally.entry(format!("Run ok; instance ever listed: {ever}")).or_default() += 1;
        if ever {
            listed_ms.push(ms(t0, last));
        }
        println!("run {i}: guid={guid} run_pid={run_pid} listed={ever} listed_ms={:.2} pids_listed={pids:?} {}", ms(t0, last), lr(&t));
    }
    println!("===== SUMMARY nostart runs={runs}");
    println!("listed duration ms: {}", stats(listed_ms));
    for (k, v) in tally {
        println!("tally: {k}: {v}");
    }
    0
}

// L1-L3. `ended <task> <exp> <runs> <label> <actlog> [churn]`
//
// Per run: Run the task, then sample the listing in a tight loop (GetInstances, EnginePID of our instance,
// OpenProcess by that pid, creation time, exit code, signalled) until nothing is listed. The first sample in which
// the process is gone (signalled) but the instance is still listed is the "ended-but-listed" window; the experiment
// `exp` acts there once:
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
    unopenable_ms: Vec<f64>,
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
    th: Option<std::thread::JoinHandle<()>>,
}

fn start_churn() -> Churn {
    let log = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (l2, s2) = (log.clone(), stop.clone());
    let exe = std::env::current_exe().unwrap();
    let th = std::thread::spawn(move || {
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
    });
    Churn { log, stop, th: Some(th) }
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

/// What the experiment does in the window. Returns a note and the second instance's guid if one was returned.
fn hook(exp: &str, path: &str, t: &IRegisteredTask, it: &IRunningTask, agg: &mut Agg) -> (String, Option<String>) {
    let fresh = task(path).ok();
    let before = format!("{} fresh[{}]", lr(t), fresh.as_ref().map(lr).unwrap_or_default());
    // SAFETY: plain COM calls.
    unsafe {
        match exp {
            "l2run" | "l2runex0" | "l2runex2" => {
                let r = match exp {
                    "l2run" => t.Run(&VARIANT::default()),
                    "l2runex0" => t.RunEx(&VARIANT::default(), 0, 0, &BSTR::new()),
                    _ => t.RunEx(&VARIANT::default(), 2, 0, &BSTR::new()),
                };
                match r {
                    Ok(rt) => {
                        let g = rt.InstanceGuid().map(|b| b.to_string()).unwrap_or_default();
                        let pid = rt.EnginePID().unwrap_or(0);
                        let st = r2s(rt.State().map(|s| s.0));
                        agg.t(format!("{exp}: call ok, pid={}", if pid == 0 { "0" } else { "nonzero" }));
                        (format!("hook[{exp} ok guid={g} pid={pid} state={st}] at-hook[{before}]"), Some(g))
                    }
                    Err(e) => {
                        agg.t(format!("{exp}: call err {}", hr(&e)));
                        (format!("hook[{exp} err {}] at-hook[{before}]", hr(&e)), None)
                    }
                }
            }
            "l3stop" => {
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

        let mut obs: Vec<(u32, u64)> = Vec::new();
        let (mut samples, mut alive, mut exited) = (0u64, 0u64, 0u64);
        let (mut pid0_first, mut pid0_last): (Option<u64>, u64) = (None, 0);
        let (mut first_pid, mut pid_changed) = (None::<u32>, false);
        let (mut opened_ok, mut hooked) = (false, false);
        let (mut exit_seen, mut exit_ft): (Option<u64>, Option<u64>) = (None, None);
        let (mut unopen_first, mut unopen_last): (Option<u64>, u64) = (None, 0);
        let mut drop1: Option<u64> = None;
        let mut note = String::new();
        let mut guid2: Option<String> = None;
        let mut seen2 = false;
        let mut other_listed = 0u32;
        let mut consec_err = 0u32;
        let mut fail_before = BTreeMap::<u32, u32>::new();
        let mut fail_after = BTreeMap::<u32, u32>::new();
        let mut codes = BTreeMap::<u32, u32>::new();

        loop {
            let list = match instances(&t) {
                Ok(l) => {
                    consec_err = 0;
                    l
                }
                Err(e) => {
                    agg.t(format!("listing error {}", hr(&e)));
                    consec_err += 1;
                    if consec_err > 100_000 {
                        break;
                    }
                    continue;
                }
            };
            let ts = now_ft();
            let mine = list.iter().find(|x| same_guid(&x.guid, &guid));
            if mine.is_none() && drop1.is_none() {
                drop1 = Some(ts);
            }
            if let Some(g2) = &guid2 {
                if list.iter().any(|x| same_guid(&x.guid, g2)) {
                    seen2 = true;
                }
            }
            if list.iter().any(|x| !same_guid(&x.guid, &guid) && guid2.as_deref().map_or(true, |g| !same_guid(&x.guid, g))) {
                other_listed += 1;
            }
            if list.is_empty() {
                // For `fire`, keep observing for the settle window after the first emptiness.
                if settle_ms > 0.0 && ms(drop1.unwrap_or(ts), ts) < settle_ms {
                    continue;
                }
                break;
            }
            if mine.is_none() && !l2 {
                break;
            }
            let Some(m) = mine else { continue };
            samples += 1;
            if m.pid == 0 {
                agg.pid0_samples += 1;
                pid0_first.get_or_insert(ts);
                pid0_last = ts;
                continue;
            }
            match first_pid {
                None => first_pid = Some(m.pid),
                Some(p) if p != m.pid => pid_changed = true,
                _ => {}
            }
            match probe_pid(m.pid) {
                Ok(p) => {
                    opened_ok = true;
                    if !obs.contains(&(m.pid, p.ct)) {
                        obs.push((m.pid, p.ct));
                    }
                    if p.exited {
                        exited += 1;
                        exit_seen.get_or_insert(ts);
                        exit_ft.get_or_insert(p.exit_ft);
                        *codes.entry(p.code).or_default() += 1;
                        if !hooked {
                            hooked = true;
                            let (n, g) = hook(exp, path, &t, &m.it, &mut agg);
                            note = n;
                            guid2 = g;
                        }
                    } else {
                        alive += 1;
                    }
                }
                Err(e) => {
                    unopen_first.get_or_insert(ts);
                    unopen_last = ts;
                    if opened_ok {
                        *fail_after.entry(e).or_default() += 1;
                    } else {
                        *fail_before.entry(e).or_default() += 1;
                    }
                    // 87 (invalid parameter) is "no such process": the process is gone yet the instance is listed.
                    if e == 87 && !hooked {
                        hooked = true;
                        let (n, g) = hook(exp, path, &t, &m.it, &mut agg);
                        note = format!("[hook on unopenable pid] {n}");
                        guid2 = g;
                    }
                }
            }
        }
        let drop1 = drop1.unwrap_or_else(now_ft);
        let after = lr(&t);

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
                agg.t("IDENTITY: listed pid differs from the action's own pid");
            }
            if rec_ct != 0 && *c != rec_ct {
                mism.push(format!("pid {p} opened with creation {c}, action's own is {rec_ct}"));
                agg.t("IDENTITY: opened process has another creation time than the action");
            }
        }
        if run_pid != 0 && rec_pid != 0 && run_pid != rec_pid {
            agg.t("Run's EnginePID differs from the action's own pid");
            mism.push(format!("Run returned pid {run_pid}, action is {rec_pid}"));
        }
        if pid_changed {
            agg.t("listed pid changed during the run");
        }
        let mut coll = String::new();
        if let Some(ch) = &ch {
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
        let linger_exit = exit_ft.map(|e| ms(e, drop1));
        if let Some(v) = linger_seen {
            agg.window_runs += 1;
            agg.linger_seen_ms.push(v);
        }
        if let Some(v) = linger_exit {
            agg.linger_exit_ms.push(v);
        }
        if let Some(f) = unopen_first {
            agg.unopenable_ms.push(ms(f, unopen_last.max(f)) + 0.0);
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
        let second = if new.len() >= 2 {
            let t2: u64 = field(new[1], "t").and_then(|v| v.parse().ok()).unwrap_or(0);
            format!("second start {:+.2} ms from the listing's drop", ms(drop1, t2))
        } else {
            String::new()
        };
        println!(
            "run {i}: guid={guid} run_pid={run_pid} rec_pid={rec_pid} run_ret_ms={:.1} samples={samples} alive={alive} exited={exited} \
             exit_seen_to_drop_ms={} exit_to_drop_ms={} pid0_ms={} unopenable_ms={} fail_before={fail_before:?} fail_after={fail_after:?} \
             starts={} second2_listed={seen2} other_listed_samples={other_listed} {second} {note} after[{after}] {} {coll}",
            ms(t_run, t_ret),
            linger_seen.map(|v| format!("{v:.2}")).unwrap_or("-".into()),
            linger_exit.map(|v| format!("{v:.2}")).unwrap_or("-".into()),
            pid0_first.map(|f| format!("{:.2}", ms(f, pid0_last))).unwrap_or("-".into()),
            unopen_first.map(|f| format!("{:.2}", ms(f, unopen_last))).unwrap_or("-".into()),
            new.len(),
            mism.join("; ")
        );
        for (k, v) in &fail_before {
            agg.t(format!("OPEN FAILED (listed pid, never opened before) win32={k} (x{v})"));
        }
        for (k, v) in &fail_after {
            agg.t(format!("OPEN FAILED (listed pid, opened earlier in this run) win32={k} (x{v})"));
        }
    }

    if let Some(mut c) = ch {
        c.stop.store(true, Ordering::Relaxed);
        if let Some(th) = c.th.take() {
            let _ = th.join();
        }
        let n = c.log.lock().unwrap().len();
        println!("churn processes created: {n}");
    }
    println!("===== SUMMARY label={label} exp={exp} runs_ok={} churn={churn}", agg.runs);
    println!("samples={} alive={} exited={} listed-pid0-samples={}", agg.samples, agg.alive, agg.exited, agg.pid0_samples);
    println!("runs where the exited-but-listed window was seen: {}", agg.window_runs);
    println!("runs where the listed pid was never openable: {}", agg.never_opened_runs);
    println!("exit_seen_to_drop_ms (first exited sample to the drop of the listing): {}", stats(agg.linger_seen_ms.clone()));
    println!("exit_to_drop_ms (the process's ExitTime to the sample that found it unlisted): {}", stats(agg.linger_exit_ms.clone()));
    println!("pid0 runs={} duration_ms: {}", agg.pid0_runs, stats(agg.pid0_ms.clone()));
    println!("unopenable-while-listed duration_ms: {}", stats(agg.unopenable_ms.clone()));
    println!("action starts per run: {:?}", agg.starts);
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
        let mut pids = BTreeMap::<u32, u32>::new();
        loop {
            let Ok(list) = instances(&t) else { continue };
            let ts = now_ft();
            match list.iter().find(|x| same_guid(&x.guid, &guid)) {
                Some(m) => {
                    ever = true;
                    last = ts;
                    *pids.entry(m.pid).or_default() += 1;
                }
                None => break,
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

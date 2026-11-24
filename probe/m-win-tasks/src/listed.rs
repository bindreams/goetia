// Is a stopped instance still listed for as long as its action process is alive? `tsprobe listed ...`, driven by
// `listed.ps1`.
//
//   listed <task-path> <running|registered> <row> <actlog> <ready-lines>
//
// `Run` the task (the action is `act ctrl` or `order parent`), wait until the action's log has <ready-lines> lines
// ending ` ready`, hold a handle to the action's process (the pid `Run` returned) and to the child's if there is one,
// start four samplers, and, once each has sampled, `Stop` the instance (`running`: `IRunningTask::Stop` on the
// object `Run` returned; `registered`: `IRegisteredTask::Stop(0)`). The samplers go on until the action process has
// ended, the listing is empty and the child has ended. A sampler is a thread that calls as fast as its calls allow:
//   held     goetia's own read on a held task: `GetInstances(0)`, `Count`, `get_Item`, `InstanceGuid`, `EnginePID`
//            (goetia's `listed`, src/backend/scm/manager/task/com.rs)
//   fresh    the same read after a new `ITaskService` connection, `GetFolder("\")` and `GetTask(path)` for every sample
//            (goetia opens a `Session` for every call)
//   hidden   the same read with `GetInstances(TASK_ENUM_HIDDEN)`
//   object   `State` and `Refresh` on the instance object `Run` returned
// Each sample is (time before the call, time after it, what the call answered, whether the action's process handle was
// signalled right after the call). One clock for everything: `GetSystemTimePreciseAsFileTime`. A process that is not
// signalled right after a call was alive during the whole call, so a call that answered "not listed" (or failed) then
// is a violation; the answer `LR` means: an entry whose `EnginePID` reads and is the action's pid.
//
// Output lines, times in ms from the call of `Stop` (negative: before it):
//   S <row> stop-returned <t> [ERROR ...]
//   P <row> <action ended> <child ended or -> <listing empty> (the end times are a watcher thread's wake-up)
//   R <row> <sampler> n=.. spacing[..] call[..] last-good=.. first-bad=.. first-signalled=.. violations=.. ...
//   V <row> <sampler> <t0> <t1> <answer>      a violation
//   T <row> <sampler> <t0> <t1> <answer> sig=<0|1>     the first sample of each run of equal (answer, signalled)
//   B <row> <role>                            a failure bound of 60 s was reached for this process
// Every wait here is on a handle or on a condition re-read, or is a printed failure bound. Throwaway CI runner only.
use crate::util::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use windows::core::*;
use windows::Win32::Foundation::SCHED_E_TASK_NOT_RUNNING;
use windows::Win32::System::TaskScheduler::*;
use windows::Win32::System::Variant::VARIANT;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Held,
    Fresh,
    Hidden,
    Object,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Held => "held",
            Kind::Fresh => "fresh",
            Kind::Hidden => "hidden",
            Kind::Object => "object",
        }
    }

    /// The answer a sampler gives while the instance is as `Run` made it.
    fn good(self, answer: &str) -> bool {
        match self {
            Kind::Object => !answer.contains("ERR"),
            _ => answer == "LR",
        }
    }
}

struct Sample {
    t0: u64,
    t1: u64,
    answer: usize,
    signalled: bool,
}

struct Series {
    kind: Kind,
    answers: Vec<String>,
    samples: Vec<Sample>,
}

/// The instance object `Run` returned, moved to a thread of the same multithreaded apartment.
struct Object(IRunningTask);
// SAFETY: a COM interface of the multithreaded apartment, used from other threads of it.
unsafe impl Send for Object {}

/// What one read of the listing of `task` answered, as one token per entry joined by `+`.
fn read_listing(task: &IRegisteredTask, flags: i32, pid: u32) -> String {
    // SAFETY: plain COM calls on live interfaces; collection items are numbered from 1.
    unsafe {
        let coll = match task.GetInstances(flags) {
            Ok(c) => c,
            Err(e) => return format!("ERR:GetInstances:{}", hr(&e)),
        };
        let n = match coll.Count() {
            Ok(n) => n,
            Err(e) => return format!("ERR:Count:{}", hr(&e)),
        };
        if n == 0 {
            return "NL".into();
        }
        let mut tokens = Vec::new();
        for i in 1..=n {
            let it = match coll.get_Item(&VARIANT::from(i)) {
                Ok(it) => it,
                Err(e) => {
                    tokens.push(format!("ERR:get_Item:{}", hr(&e)));
                    continue;
                }
            };
            if let Err(e) = it.InstanceGuid() {
                tokens.push(format!("ERR:InstanceGuid:{}", hr(&e)));
                continue;
            }
            tokens.push(match it.EnginePID() {
                Ok(p) if p == pid => "LR".to_owned(),
                Ok(p) => format!("LR-other:{p}"),
                // The one failure the end of a run has; goetia reads it as "ended" and leaves the entry out.
                Err(e) if e.code() == SCHED_E_TASK_NOT_RUNNING => format!("LU:{}", hr(&e)),
                Err(e) => format!("ERR:EnginePID:{}", hr(&e)),
            });
        }
        tokens.join("+")
    }
}

/// goetia's `Session::open` and `with_task`: a connection, the root folder, the task by its path.
fn fresh_task(path: &str) -> std::result::Result<IRegisteredTask, String> {
    let svc = connect().map_err(|e| format!("ERR:connect:{}", hr(&e)))?;
    // SAFETY: plain COM calls on live interfaces.
    unsafe {
        let root = svc.GetFolder(&BSTR::from("\\")).map_err(|e| format!("ERR:GetFolder:{}", hr(&e)))?;
        root.GetTask(&BSTR::from(path)).map_err(|e| format!("ERR:GetTask:{}", hr(&e)))
    }
}

fn sampler(kind: Kind, path: String, pid: u32, object: Option<Object>, count: Arc<AtomicU64>, stop: Arc<AtomicBool>) -> Series {
    com_init();
    let held = Held::open(pid).expect("open the action's process");
    let task = match kind {
        Kind::Held | Kind::Hidden => Some(task(&path).expect("get the task")),
        _ => None,
    };
    let mut table: HashMap<String, usize> = HashMap::new();
    let mut series = Series { kind, answers: Vec::new(), samples: Vec::new() };
    loop {
        let t0 = now_ft();
        let answer = match kind {
            Kind::Held => read_listing(task.as_ref().unwrap(), 0, pid),
            Kind::Hidden => read_listing(task.as_ref().unwrap(), TASK_ENUM_HIDDEN.0, pid),
            Kind::Fresh => match fresh_task(&path) {
                Ok(t) => read_listing(&t, 0, pid),
                Err(e) => e,
            },
            Kind::Object => {
                let it = &object.as_ref().unwrap().0;
                // SAFETY: plain COM calls on a live interface.
                let state = match unsafe { it.State() } {
                    Ok(s) => format!("S={}", s.0),
                    Err(e) => format!("S=ERR:{}", hr(&e)),
                };
                let refresh = match unsafe { it.Refresh() } {
                    Ok(()) => "R=ok".to_owned(),
                    Err(e) => format!("R=ERR:{}", hr(&e)),
                };
                format!("{state} {refresh}")
            }
        };
        // Not signalled right after the call: the process was alive during all of it.
        let signalled = held.signalled();
        let t1 = now_ft();
        let next = table.len();
        let id = *table.entry(answer.clone()).or_insert(next);
        if id == series.answers.len() {
            series.answers.push(answer);
        }
        series.samples.push(Sample { t0, t1, answer: id, signalled });
        count.fetch_add(1, Ordering::Release);
        if stop.load(Ordering::Acquire) {
            return series;
        }
    }
}

fn ready_lines(log: &str) -> usize {
    lines(log).iter().filter(|l| l.ends_with(" ready")).count()
}

/// The child's pid out of the log's `child-spawned pid=<n>` line (the last `pid=` of it).
fn child_pid(log: &str) -> Option<u32> {
    let line = lines(log).into_iter().find(|l| l.contains("child-spawned"))?;
    line.split_whitespace().filter_map(|w| w.strip_prefix("pid=")).last()?.parse().ok()
}

/// A thread that waits on the process and stamps the clock as it wakes.
fn watch(pid: u32) -> std::thread::JoinHandle<u64> {
    std::thread::spawn(move || {
        let h = Held::open(pid).expect("open a process to watch");
        h.wait(u32::MAX);
        now_ft()
    })
}

pub fn run(a: &[String]) -> i32 {
    let (path, via, row, log) = (&a[0], a[1].as_str(), &a[2], &a[3]);
    let ready: usize = a[4].parse().expect("ready-lines");
    com_init();
    let t = task(path).expect("get the task");
    // SAFETY: plain COM calls on live interfaces.
    let rt = match unsafe { t.Run(&VARIANT::default()) } {
        Ok(rt) => rt,
        Err(e) => {
            println!("S {row} run-failed {}", hr(&e));
            return 3;
        }
    };
    let pid = match unsafe { rt.EnginePID() } {
        Ok(p) if p != 0 => p,
        other => {
            println!("S {row} run-returned-no-pid {other:?}");
            return 3;
        }
    };
    while ready_lines(log) < ready {
        std::thread::yield_now();
    }
    let action = Held::open_killable(pid).expect("open the action's process");
    let child = child_pid(log).map(|p| (p, Held::open_killable(p).expect("open the child's process")));
    let action_end = watch(pid);
    let child_end = child.as_ref().map(|(p, _)| watch(*p));

    let stop = Arc::new(AtomicBool::new(false));
    let mut counts = Vec::new();
    let mut samplers = Vec::new();
    for kind in [Kind::Held, Kind::Fresh, Kind::Hidden, Kind::Object] {
        let count = Arc::new(AtomicU64::new(0));
        counts.push(count.clone());
        let (path, stop) = (path.clone(), stop.clone());
        let object = (kind == Kind::Object).then(|| Object(rt.clone()));
        samplers.push(std::thread::spawn(move || sampler(kind, path, pid, object, count, stop)));
    }
    // Every sampler has sampled the running instance: a wait on the real condition.
    while counts.iter().any(|c| c.load(Ordering::Acquire) < 100) {
        std::thread::yield_now();
    }

    let t0 = now_ft();
    // SAFETY: plain COM calls on live interfaces.
    let stopped = match via {
        "running" => unsafe { rt.Stop() },
        "registered" => unsafe { t.Stop(0) },
        other => panic!("unknown stop {other}"),
    };
    let t_ret = now_ft();
    println!(
        "S {row} stop-returned {:.2}{}",
        ms(t0, t_ret),
        stopped.as_ref().err().map(|e| format!(" ERROR {}", hr(e))).unwrap_or_default()
    );

    // A failure bound of 60 s, printed: the action should be ended by Windows within about a second (or, for a
    // child, five).
    if stopped.is_err() || !action.wait(60_000) {
        println!("B {row} action");
        action.kill();
        action.wait(10_000);
    }
    let a_end = action_end.join().expect("action watcher");
    // The listing is empty: a wait on the real condition, no cap.
    loop {
        if let Ok(c) = unsafe { t.GetInstances(0).and_then(|c| c.Count()) } {
            if c == 0 {
                break;
            }
        }
    }
    let t_drop = now_ft();
    if let Some((_, h)) = &child {
        if !h.wait(60_000) {
            println!("B {row} child");
            h.kill();
            h.wait(10_000);
        }
    }
    let c_end = child_end.map(|w| w.join().expect("child watcher"));
    stop.store(true, Ordering::Release);

    println!(
        "P {row} {:.2} {} {:.2}",
        ms(t0, a_end),
        c_end.map_or("-".to_owned(), |c| format!("{:.2}", ms(t0, c))),
        ms(t0, t_drop)
    );
    for s in samplers {
        report(row, &s.join().expect("sampler"), t0, a_end);
    }
    0
}

fn report(row: &str, s: &Series, t0: u64, a_end: u64) {
    let name = s.kind.name();
    let ans = |x: &Sample| s.answers[x.answer].as_str();
    let rel = |t: u64| ms(t0, t);
    let spacing: Vec<f64> = s.samples.windows(2).map(|w| ms(w[0].t0, w[1].t0)).collect();
    let call: Vec<f64> = s.samples.iter().map(|x| ms(x.t0, x.t1)).collect();
    let pre = s.samples.iter().filter(|x| x.t1 < t0).count();
    let last_good = s.samples.iter().rposition(|x| s.kind.good(ans(x)));
    let describe = |x: &Sample| format!("[{:.2},{:.2}] {} sig={}", rel(x.t0), rel(x.t1), ans(x), x.signalled as u8);
    let last = last_good.map_or("none".to_owned(), |i| describe(&s.samples[i]));
    let first_bad = last_good
        .map_or(s.samples.first(), |i| s.samples.get(i + 1))
        .map_or("none".to_owned(), |x| describe(x));
    let first_sig = s.samples.iter().find(|x| x.signalled).map_or("none".to_owned(), |x| format!("{:.2}", rel(x.t0)));
    let violations: Vec<&Sample> = s.samples.iter().filter(|x| !x.signalled && !s.kind.good(ans(x))).collect();
    let good_after_sig = s.samples.iter().filter(|x| x.signalled && s.kind.good(ans(x))).count();
    println!(
        "R {row} {name} n={} pre-stop={pre} spacing[{}] call[{}] last-good={last} first-bad-after={first_bad} \
         first-signalled-sample={first_sig} action-ended={:.2} violations={} good-after-signalled={good_after_sig} answers={}",
        s.samples.len(),
        stats(spacing),
        stats(call),
        rel(a_end),
        violations.len(),
        s.answers.join(" | ")
    );
    for x in violations.iter().take(20) {
        println!("V {row} {name} {:.2} {:.2} {}", rel(x.t0), rel(x.t1), ans(x));
    }
    let mut prev: Option<(usize, bool)> = None;
    for x in &s.samples {
        if prev != Some((x.answer, x.signalled)) {
            println!("T {row} {name} {:.2} {:.2} {} sig={}", rel(x.t0), rel(x.t1), ans(x), x.signalled as u8);
            prev = Some((x.answer, x.signalled));
        }
    }
}

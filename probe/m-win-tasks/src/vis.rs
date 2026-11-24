// V1: is a run that its own action's GetInstances lists visible, at that moment, to another process's GetInstances?
//   tsprobe vis <task-path> <dir> <rounds>
// The reader of the experiment. Round n: the action (`act own2`) lists itself through its own COM connection, then
// announces by renaming `<dir>\r<n>.announce` into place (pid, guid, the FILETIME at which its own listing returned).
// This process, a separate one with its own COM connections, spins on that file and the moment it appears reads the
// task's listing two ways, until the announced instance is in it:
//   A  one long-lived ITaskService (connected once, before round 1): GetFolder, GetTask, GetInstances per read;
//   B  a fresh connection per read (Connect, GetFolder, GetTask, GetInstances), as goetia's `instances` does.
// It then writes `<dir>\r<n>.done`. A round the action could not announce has `<dir>\r<n>.skip`.
// `OBS_BOUND_S` is the observation bound of a negative read (an instance still unseen that long after its
// announcement is reported "never seen"); it is a measurement parameter, not a synchronisation.
use crate::util::*;
use windows::core::*;
use windows::Win32::System::TaskScheduler::*;

const OBS_BOUND_S: f64 = 60.0;

fn exists(p: &str) -> bool {
    std::fs::metadata(p).is_ok()
}

fn read_long(svc: &ITaskService, path: &str) -> Result<Vec<Inst>> {
    let (folder, name) = path.rsplit_once('\\').expect("task path with a folder");
    // SAFETY: plain COM calls.
    let t = unsafe { svc.GetFolder(&BSTR::from(folder))?.GetTask(&BSTR::from(name))? };
    instances(&t)
}

fn read_fresh(path: &str) -> Result<Vec<Inst>> {
    task(path).and_then(|t| instances(&t))
}

fn judge(r: Result<Vec<Inst>>, guid: &str) -> std::result::Result<bool, String> {
    match r {
        Ok(l) => Ok(l.iter().any(|i| same_guid(&i.guid, guid))),
        Err(e) => Err(hr(&e)),
    }
}

#[derive(Default)]
struct Track {
    first: String,
    first_t0: u64,
    found: bool,
    polls: u32,
    found_polls: u32,
    present_t0: u64,
    present_t1: u64,
    errors: Vec<String>,
}

impl Track {
    fn step(&mut self, r: std::result::Result<bool, String>, t0: u64, t1: u64) {
        self.polls += 1;
        let label = match &r {
            Ok(true) => "present".to_string(),
            Ok(false) => "MISSED".to_string(),
            Err(e) => format!("error {e}"),
        };
        if self.polls == 1 {
            self.first = label;
            self.first_t0 = t0;
        }
        match r {
            Ok(true) => {
                self.found = true;
                self.found_polls = self.polls;
                self.present_t0 = t0;
                self.present_t1 = t1;
            }
            Ok(false) => {}
            Err(e) => self.errors.push(e),
        }
    }

    fn line(&self, name: &str, ann: u64) -> String {
        if self.found {
            format!(
                "{name} first={} polls_to_present={} first_read_at={:.3}ms present_read={:.3}..{:.3}ms errs={}",
                self.first,
                self.found_polls,
                ms(ann, self.first_t0),
                ms(ann, self.present_t0),
                ms(ann, self.present_t1),
                self.errors.len()
            )
        } else {
            format!("{name} first={} NEVER-SEEN polls={} errs={:?}", self.first, self.polls, self.errors)
        }
    }
}

#[derive(Default)]
struct Tally {
    present: u32,
    missed: u32,
    err: u32,
    never: u32,
    miss_end_ms: Vec<f64>,
    miss_span_ms: Vec<f64>,
    polls_of_misses: Vec<f64>,
}

impl Tally {
    fn add(&mut self, t: &Track, ann: u64) {
        if t.first == "present" {
            self.present += 1;
        } else if t.first == "MISSED" {
            self.missed += 1;
        } else {
            self.err += 1;
        }
        if !t.found {
            self.never += 1;
        } else if t.first != "present" {
            self.miss_end_ms.push(ms(ann, t.present_t1));
            self.miss_span_ms.push(ms(t.first_t0, t.present_t0));
            self.polls_of_misses.push(t.found_polls as f64);
        }
    }

    fn show(&self, name: &str) {
        println!(
            "SUMMARY {name}: first read present={} MISSED={} errored={}; never seen within {OBS_BOUND_S} s={}",
            self.present, self.missed, self.err, self.never
        );
        println!("SUMMARY {name}: rounds that did not see it first, announcement to the end of the first present read, ms: {}", stats(self.miss_end_ms.clone()));
        println!("SUMMARY {name}: rounds that did not see it first, start of first read to start of first present read, ms: {}", stats(self.miss_span_ms.clone()));
        println!("SUMMARY {name}: rounds that did not see it first, reads until present: {}", stats(self.polls_of_misses.clone()));
    }
}

pub fn run(a: &[String]) -> i32 {
    let path = &a[0];
    let dir = &a[1];
    let rounds: u32 = a[2].parse().expect("rounds");
    com_init();
    let svc = connect().expect("connect");
    let (mut ta, mut tb) = (Tally::default(), Tally::default());
    let (mut skipped, mut seen_ms) = (0u32, Vec::new());
    for n in 1..=rounds {
        let ann_path = format!("{dir}\\r{n}.announce");
        let skip_path = format!("{dir}\\r{n}.skip");
        let skipped_now = loop {
            if exists(&ann_path) {
                break false;
            }
            if exists(&skip_path) {
                break true;
            }
            std::hint::spin_loop();
        };
        if skipped_now {
            skipped += 1;
            println!("R {n} skipped: the action did not announce: {:?}", lines(&format!("{dir}\\r{n}.act")));
            let _ = std::fs::write(format!("{dir}\\r{n}.done"), "skipped");
            continue;
        }
        let seen = now_ft();
        let text = std::fs::read_to_string(&ann_path).unwrap_or_default();
        let guid = field(&text, "guid").unwrap_or("").to_string();
        let pid = field(&text, "pid").unwrap_or("0").to_string();
        let ann: u64 = field(&text, "t").and_then(|s| s.parse().ok()).unwrap_or(seen);
        seen_ms.push(ms(ann, seen));
        let (mut ra, mut rb) = (Track::default(), Track::default());
        loop {
            if !ra.found {
                let t0 = now_ft();
                let r = judge(read_long(&svc, path), &guid);
                ra.step(r, t0, now_ft());
            }
            if !rb.found {
                let t0 = now_ft();
                let r = judge(read_fresh(path), &guid);
                rb.step(r, t0, now_ft());
            }
            if (ra.found && rb.found) || ms(ann, now_ft()) > OBS_BOUND_S * 1000.0 {
                break;
            }
        }
        println!("R {n} pid={pid} guid={guid} file_seen={:.3}ms | {} | {}", ms(ann, seen), ra.line("A", ann), rb.line("B", ann));
        ta.add(&ra, ann);
        tb.add(&rb, ann);
        let _ = std::fs::write(format!("{dir}\\r{n}.done"), "done");
    }
    println!("SUMMARY rounds={rounds} announced={} skipped={skipped}", rounds - skipped);
    println!("SUMMARY announcement to the reader seeing the file, ms: {}", stats(seen_ms));
    ta.show("A (long-lived connection)");
    tb.show("B (fresh connection per read)");
    0
}

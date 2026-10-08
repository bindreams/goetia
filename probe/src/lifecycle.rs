//! One row end to end: fixture, verdicts, launch, observe, record, tear down.

use std::fs;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::candidates::{self, MechResult, Target};
use crate::fixtures::{self, Spec};
use crate::sys;

pub fn base(run_id: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/goetia-probe-{run_id}"))
}

fn phase(id: &str, s: &str) {
    let line = format!("phase={s}\n");
    print!("{line}");
    let _ = std::io::stdout().flush();
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(format!("results/{id}.phase")) {
        let _ = f.write_all(line.as_bytes());
    }
}

// FIFOs -------------------------------------------------------------------------------------------

fn open_raw(p: &Path, flags: i32) -> Result<OwnedFd, i32> {
    let c = sys::cpath(p);
    let fd = unsafe { libc::open(c.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 { Err(sys::errno()) } else { Ok(unsafe { OwnedFd::from_raw_fd(fd) }) }
}

/// A FIFO with a held non-blocking reader and a held writer (H4a/b).
struct Fifo {
    r: OwnedFd,
    w: Option<OwnedFd>,
}

fn make_fifo(p: &Path, anomalies: &mut Vec<String>) -> Option<Fifo> {
    let c = sys::cpath(p);
    if unsafe { libc::mkfifo(c.as_ptr(), 0o666) } != 0 {
        anomalies.push(format!("mkfifo {}: {}", p.display(), sys::errno()));
        return None;
    }
    let r = open_raw(p, libc::O_RDONLY | libc::O_NONBLOCK).ok()?;
    if unsafe { libc::fchmod(r.as_raw_fd(), 0o666) } != 0 {
        anomalies.push(format!("fchmod {}: {}", p.display(), sys::errno()));
    }
    let mode = fs::metadata(p).map(|m| m.mode() & 0o7777).unwrap_or(0);
    if mode != 0o666 {
        anomalies.push(format!("{} mode {mode:o}, want 666", p.display()));
    }
    let w = open_raw(p, libc::O_WRONLY).ok();
    if w.is_none() {
        anomalies.push(format!("open writer {}", p.display()));
    }
    Some(Fifo { r, w })
}

/// Everything readable right now, never blocking.
fn drain(fd: &OwnedFd, into: &mut Vec<u8>) {
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        into.extend_from_slice(&buf[..n as usize]);
    }
}

// Plist / launchctl --------------------------------------------------------------------------------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// goetia's `restart: never` shape and key order (`generate.rs:330-356`).
fn plist(label: &str, args: &[String], cwd: Option<&Path>, user: &str, log: Option<&Path>) -> String {
    let mut d = format!("<key>Label</key><string>{}</string>\n<key>ProgramArguments</key><array>", esc(label));
    for a in args {
        d += &format!("<string>{}</string>", esc(a));
    }
    d += "</array>\n";
    if let Some(c) = cwd {
        d += &format!("<key>WorkingDirectory</key><string>{}</string>\n", esc(&c.to_string_lossy()));
    }
    d += &format!("<key>UserName</key><string>{}</string>\n", esc(user));
    if let Some(l) = log {
        let l = esc(&l.to_string_lossy());
        d += &format!("<key>StandardOutPath</key><string>{l}</string>\n<key>StandardErrorPath</key><string>{l}</string>\n");
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n{d}</dict>\n</plist>\n"
    )
}

fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key} = ");
    text.lines().find_map(|l| l.trim_start().strip_prefix(prefix.as_str()))
}

fn print_job(label: &str) -> String {
    fixtures::run("launchctl", &["print", &format!("system/{label}")]).1
}

// kqueue ---------------------------------------------------------------------------------------------

struct Kq(OwnedFd);

impl Kq {
    fn new() -> Kq {
        let fd = unsafe { libc::kqueue() };
        assert!(fd >= 0, "kqueue");
        Kq(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Register with `EV_RECEIPT`; returns the per-change errno (0 = attached).
    fn add(&self, ident: usize, filter: i16, fflags: u32) -> i64 {
        let ch = libc::kevent {
            ident,
            filter,
            flags: libc::EV_ADD | libc::EV_RECEIPT,
            fflags,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let mut out: libc::kevent = unsafe { std::mem::zeroed() };
        let n = unsafe { libc::kevent(self.0.as_raw_fd(), &ch, 1, &mut out, 1, std::ptr::null()) };
        if n != 1 || out.flags & libc::EV_ERROR == 0 {
            return -1;
        }
        out.data as i64
    }

    /// Next batch of events; `None` when `bound` elapsed (F29 only).
    fn wait(&self, bound: Option<Duration>) -> Option<Vec<libc::kevent>> {
        let mut out: [libc::kevent; 4] = unsafe { std::mem::zeroed() };
        let ts = bound.map(|b| libc::timespec {
            tv_sec: b.as_secs() as libc::time_t,
            tv_nsec: 0,
        });
        let tsp = ts.as_ref().map_or(std::ptr::null(), |t| t as *const _);
        loop {
            let n = unsafe { libc::kevent(self.0.as_raw_fd(), std::ptr::null(), 0, out.as_mut_ptr(), 4, tsp) };
            if n < 0 && sys::errno() == libc::EINTR {
                continue;
            }
            assert!(n >= 0, "kevent wait errno {}", sys::errno());
            return (n > 0).then(|| out[..n as usize].to_vec());
        }
    }
}

/// Wait for our own child, optionally bounded. Register, then `try_wait` (reaps if already done),
/// then wait: a child not yet exited at `try_wait` exits after the registration.
fn wait_child(child: &mut std::process::Child, bound: Option<Duration>) -> Option<std::process::ExitStatus> {
    let kq = Kq::new();
    let r = kq.add(child.id() as usize, libc::EVFILT_PROC, libc::NOTE_EXIT);
    if let Ok(Some(st)) = child.try_wait() {
        return Some(st);
    }
    if r != 0 {
        return child.wait().ok();
    }
    kq.wait(bound)?;
    child.wait().ok()
}

// The row -----------------------------------------------------------------------------------------

#[derive(Serialize, Default)]
struct LogObs {
    lstat_type: Option<String>,
    uid: Option<u32>,
    gid: Option<u32>,
    mode: Option<String>,
    dev: Option<u64>,
    ino: Option<u64>,
    rdev: Option<u64>,
    content: Option<String>,
    ls: Option<String>,
}

fn observe(p: &Path) -> LogObs {
    let mut o = LogObs::default();
    let Ok(m) = fs::symlink_metadata(p) else {
        o.lstat_type = Some("absent".into());
        return o;
    };
    let ft = m.file_type();
    o.lstat_type = Some(
        if ft.is_file() {
            "file"
        } else if ft.is_dir() {
            "dir"
        } else if ft.is_symlink() {
            "symlink"
        } else if m.mode() & libc::S_IFMT as u32 == libc::S_IFIFO as u32 {
            "fifo"
        } else if m.mode() & libc::S_IFMT as u32 == libc::S_IFCHR as u32 {
            "chr"
        } else {
            "other"
        }
        .into(),
    );
    o.uid = Some(m.uid());
    o.gid = Some(m.gid());
    o.mode = Some(format!("{:o}", m.mode() & 0o7777));
    o.dev = Some(m.dev());
    o.ino = Some(m.ino());
    o.rdev = Some(m.rdev());
    // Content only for regular files (H4); FIFOs go through the held reader.
    if ft.is_file() {
        o.content = fs::read(p).ok().map(|b| String::from_utf8_lossy(&b[..b.len().min(65536)]).into_owned());
    }
    o.ls = Some(fixtures::run("ls", &["-leOd", &p.to_string_lossy()]).1);
    o
}

fn cwd_from_kernel(pid: i32) -> Value {
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, (&mut info as *mut libc::proc_vnodepathinfo).cast(), size) };
    if n != size {
        return json!({ "error": format!("proc_pidinfo returned {n}, want {size}, errno {}", sys::errno()) });
    }
    let path = unsafe { std::ffi::CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
    json!({
        "dev": info.pvi_cdir.vip_vi.vi_stat.vst_dev,
        "ino": info.pvi_cdir.vip_vi.vi_stat.vst_ino,
        "path": path.to_string_lossy(),
    })
}

pub fn run_row(id: &str, run_id: &str) -> i32 {
    fs::create_dir_all("results").expect("results dir");
    let base = base(run_id);
    let bin = base.join("bin");
    let row = base.join(id);
    let fx = row.join("fx");
    let sync = row.join("sync");
    for d in [&row, &fx, &sync] {
        fs::create_dir(d).unwrap_or_else(|e| panic!("mkdir {}: {e}", d.display()));
        fs::set_permissions(d, std::os::unix::fs::PermissionsExt::from_mode(0o755)).expect("chmod");
    }
    phase(id, "fixture");
    let spec = fixtures::build(id, run_id, &fx);
    let mut anomalies = spec.anomalies.clone();
    let mut rec = json!({
        "id": id, "kind": spec.kind, "must_run": spec.must_run,
        "account": { "name": spec.acct.name, "uid": spec.acct.uid, "gid": spec.acct.gid },
        "cwd": spec.cwd, "log": spec.log, "facts": spec.facts,
        "targets": spec.targets, "job_needs": spec.job_needs,
    });

    if let Some(why) = &spec.unavailable {
        rec["status"] = json!("unavailable");
        rec["unavailable"] = json!(why);
        return finish(id, run_id, rec, anomalies, &spec, &row, None);
    }

    // Sync FIFOs and the account's reach to them (H-4).
    let status = make_fifo(&sync.join("STATUS"), &mut anomalies);
    let ctrl = make_fifo(&sync.join("CTRL"), &mut anomalies);
    let (Some(mut status), Some(mut ctrl)) = (status, ctrl) else {
        return finish(id, run_id, rec, anomalies, &spec, &row, None);
    };
    let reach = candidates::helper(
        &bin.join("launchd-probe"),
        &spec.acct,
        &[
            Target { need: "Exec".into(), bits: sys::X, path: bin.join("job") },
            Target { need: "Write".into(), bits: sys::W, path: sync.join("STATUS") },
            Target { need: "Read".into(), bits: sys::R, path: sync.join("CTRL") },
        ],
    );
    if reach.verdicts.iter().any(|v| !matches!(v, candidates::Verdict::Errno(0))) {
        anomalies.push(format!("account cannot reach job/STATUS/CTRL: {:?}", reach.verdicts));
    }

    let log_reader = if spec.hold_log_reader {
        spec.log.as_deref().and_then(|l| open_raw(l, libc::O_RDONLY | libc::O_NONBLOCK).ok())
    } else {
        None
    };
    rec["log_before"] = json!(spec.log.as_deref().map(observe));
    rec["cwd_stat"] = json!(spec.cwd.as_deref().map(observe));

    // Verdicts on the untouched fixture, before bootstrap.
    let mut mechs: Vec<MechResult> = Vec::new();
    if !spec.targets.is_empty() {
        mechs.push(candidates::root(&spec.targets));
        mechs.push(candidates::thread(&spec.acct, &spec.targets));
        mechs.push(candidates::helper(&bin.join("launchd-probe"), &spec.acct, &spec.targets));
        mechs.push(candidates::fork_initgroups(&spec.acct, &spec.targets));
        if spec.run_static {
            mechs.push(candidates::static_control(&spec.acct, &spec.targets));
        }
    }
    rec["mechanisms"] = json!(mechs);

    // Launch.
    let label = format!("com.goetia.probe.{run_id}.{id}");
    let plist_path = PathBuf::from(format!("/Library/LaunchDaemons/{label}.plist"));
    let mut args = vec![
        bin.join("job").to_string_lossy().into_owned(),
        sync.join("STATUS").to_string_lossy().into_owned(),
        sync.join("CTRL").to_string_lossy().into_owned(),
        format!("N1-{run_id}-{id}"),
        format!("N2-{run_id}-{id}"),
    ];
    for n in &spec.job_needs {
        args.push(n.bits.to_string());
        args.push(n.path.to_string_lossy().into_owned());
    }
    let body = plist(&label, &args, spec.cwd.as_deref(), &spec.acct.name, spec.log.as_deref());
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&plist_path)
        .and_then(|mut f| f.write_all(body.as_bytes()))
        .expect("write plist");
    rec["plist"] = json!(body);
    phase(id, "pre-bootstrap");
    let (boot_ok, boot_out) = fixtures::run("launchctl", &["bootstrap", "system", &plist_path.to_string_lossy()]);
    phase(id, "post-bootstrap");
    if !boot_ok {
        rec["outcome"] = json!({ "bootstrap_refused": boot_out });
        return finish(id, run_id, rec, anomalies, &spec, &row, Some((&label, &plist_path)));
    }

    // kickstart -p, bounded only for F29 (launchd is the external party).
    let bound = spec.bounded_wait.then(|| Duration::from_secs(8 * 60));
    let mut ks = Command::new("launchctl")
        .args(["kickstart", "-p", &format!("system/{label}")])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kickstart");
    let mut released_log = None;
    let ks_status = match wait_child(&mut ks, bound) {
        Some(st) => st,
        None => {
            phase(id, "bound-fired-in-kickstart");
            rec["f29_bound"] = json!(bound_evidence(id, &label, ks.id() as i32));
            released_log = spec.log.as_deref().and_then(|l| open_raw(l, libc::O_RDONLY | libc::O_NONBLOCK).ok());
            phase(id, "released-log-fifo");
            wait_child(&mut ks, None).expect("kickstart exit")
        }
    };
    let mut ks_out = String::new();
    ks.stdout.take().unwrap().read_to_string(&mut ks_out).ok();
    let mut ks_err = String::new();
    ks.stderr.take().unwrap().read_to_string(&mut ks_err).ok();
    rec["kickstart"] = json!({ "status": format!("{ks_status:?}"), "stdout": ks_out, "stderr": ks_err });
    let pid = ks_out.strip_suffix('\n').and_then(|d| d.parse::<i32>().ok());
    phase(id, &format!("post-kickstart pid={}", pid.map_or("none".into(), |p| p.to_string())));
    let Some(pid) = pid else {
        rec["outcome"] = json!({ "spawn_refused": true, "print": print_job(&label) });
        if spec.must_run {
            anomalies.push("must-run row: no pid".into());
        }
        return finish(id, run_id, rec, anomalies, &spec, &row, Some((&label, &plist_path)));
    };

    // Watch P (H3): attach with EV_RECEIPT; ESRCH = already gone; other errors anomalous.
    let kq = Kq::new();
    let proc_r = kq.add(pid as usize, libc::EVFILT_PROC, libc::NOTE_EXIT | libc::NOTE_EXITSTATUS);
    let read_r = kq.add(status.r.as_raw_fd() as usize, libc::EVFILT_READ, 0);
    if read_r != 0 {
        anomalies.push(format!("EVFILT_READ receipt {read_r}"));
    }
    let mut status_buf = Vec::new();
    let mut ready: Option<String> = None;
    let mut exit: Option<i32> = None;
    let mut printed = None;
    if proc_r == libc::ESRCH as i64 {
        drain(&status.r, &mut status_buf);
        printed = Some(print_job(&label));
    } else if proc_r != 0 {
        anomalies.push(format!("EVFILT_PROC receipt {proc_r}"));
    } else {
        let p = print_job(&label);
        let confirmed = match field(&p, "pid").map(str::trim) {
            Some(x) if x == pid.to_string() => true,
            None => {
                // Ended before we could confirm the attach was on it: never wait on it.
                drain(&status.r, &mut status_buf);
                printed = Some(p.clone());
                false
            }
            Some(other) => {
                anomalies.push(format!("print pid {other} != kickstart pid {pid}"));
                false
            }
        };
        if confirmed {
            phase(id, "pre-wait");
            let mut bound = bound;
            loop {
                let Some(evs) = kq.wait(bound) else {
                    phase(id, "bound-fired-in-wait");
                    rec["f29_bound"] = json!(bound_evidence(id, &label, pid));
                    released_log = spec.log.as_deref().and_then(|l| open_raw(l, libc::O_RDONLY | libc::O_NONBLOCK).ok());
                    phase(id, "released-log-fifo");
                    bound = None;
                    continue;
                };
                let mut got_exit = None;
                for e in &evs {
                    if e.filter == libc::EVFILT_PROC && e.fflags & libc::NOTE_EXIT != 0 {
                        got_exit = Some(e.data as i32);
                    }
                }
                // Always drain STATUS before classifying (H3), accumulating to '\n' (M5).
                drain(&status.r, &mut status_buf);
                if ready.is_none() {
                    if let Some(nl) = status_buf.iter().position(|b| *b == b'\n') {
                        let line = String::from_utf8_lossy(&status_buf[..nl]).into_owned();
                        if !line.starts_with(&format!("ready pid={pid} ")) {
                            anomalies.push(format!("ready line from wrong pid: {line}"));
                        }
                        if line.contains(" dot=err:") {
                            rec["cwd_kernel"] = cwd_from_kernel(pid);
                        }
                        ready = Some(line);
                        phase(id, "ready; releasing");
                        ctrl.w = None; // release: the job's read() sees EOF
                    }
                }
                if let Some(st) = got_exit {
                    exit = Some(st);
                    break;
                }
            }
            if ready.is_none() {
                printed = Some(print_job(&label));
            }
        }
    }
    drop(ctrl);
    if ready.is_none() {
        if let Some(nl) = status_buf.iter().position(|b| *b == b'\n') {
            ready = Some(String::from_utf8_lossy(&status_buf[..nl]).into_owned());
            anomalies.push("ready line found only after the job was already gone".into());
        }
    }
    rec["status_raw"] = json!(String::from_utf8_lossy(&status_buf));
    rec["ready"] = json!(ready);
    rec["exit"] = json!(exit.map(sys::decode_status));
    rec["print_after"] = json!(printed.as_deref().map(|p| json!({
        "last_exit_code": field(p, "last exit code"),
        "state": field(p, "state"),
    })));
    if let Some(code) = exit.and_then(sys::exited_code) {
        if code >= 200 {
            anomalies.push(format!("job-internal failure, exit {code}"));
        }
    }
    if ready.is_none() && spec.must_run {
        anomalies.push("must-run row: no ready line".into());
    }
    rec["outcome"] = json!(if ready.is_some() { "ran" } else { "failed_before_ready" });

    // Observations after exit.
    rec["log_after"] = json!(spec.log.as_deref().map(observe));
    if id == "F23l" {
        rec["target_after"] = json!(observe(&fx.join("t/target.log")));
    }
    if id == "F24l" {
        rec["dev_null"] = json!(observe(Path::new("/dev/null")));
    }
    for (name, fd) in [("log_reader", log_reader.as_ref()), ("released_reader", released_log.as_ref())] {
        if let Some(fd) = fd {
            let mut b = Vec::new();
            drain(fd, &mut b);
            rec[name] = json!(String::from_utf8_lossy(&b));
        }
    }
    status.w = None;
    finish(id, run_id, rec, anomalies, &spec, &row, Some((&label, &plist_path)))
}

/// F29: when the bound fires, sample the stuck processes before releasing the FIFO.
fn bound_evidence(id: &str, label: &str, pid: i32) -> Value {
    let p = print_job(label);
    let job_pid = field(&p, "pid").map(str::trim).map(String::from);
    let mut out = json!({ "print": p });
    for (name, target) in [("kickstart_or_job", Some(pid.to_string())), ("job", job_pid), ("launchd", Some("1".into()))] {
        if let Some(t) = target {
            let file = format!("results/{id}.sample.{name}.txt");
            out[name] = json!(fixtures::run("sample", &[&t, "1", "-file", &file]));
        }
    }
    out
}

fn finish(
    id: &str,
    _run_id: &str,
    mut rec: Value,
    anomalies: Vec<String>,
    spec: &Spec,
    row: &Path,
    job: Option<(&str, &Path)>,
) -> i32 {
    rec["anomalies"] = json!(anomalies);
    // Record before teardown (H5).
    let text = serde_json::to_string(&rec).expect("json");
    fs::write(format!("results/{id}.json"), &text).expect("write results");
    println!("RESULT {text}");
    if let Ok(p) = std::env::var("GITHUB_STEP_SUMMARY") {
        if let Ok(mut f) = fs::OpenOptions::new().append(true).open(p) {
            let _ = writeln!(
                f,
                "| {id} | {} | {} | {} |",
                rec["outcome"],
                rec["ready"].as_str().map_or("-", |r| if r.len() > 80 { &r[..80] } else { r }),
                rec["anomalies"]
            );
        }
    }
    let mut td = teardown(spec, row, job);
    td.extend(fixtures::delete_accounts(spec));
    if !td.is_empty() {
        eprintln!("teardown failures: {td:?}");
    }
    if anomalies.is_empty() && td.is_empty() { 0 } else { 1 }
}

fn teardown(spec: &Spec, row: &Path, job: Option<(&str, &Path)>) -> Vec<String> {
    let mut errs = Vec::new();
    if let Some((label, plist)) = job {
        let (ok, out) = fixtures::run("launchctl", &["bootout", &format!("system/{label}")]);
        if !ok && !out.contains("No such process") && !out.contains("Could not find service") {
            errs.push(format!("bootout: {out}"));
        }
        if let Err(e) = fs::remove_file(plist) {
            errs.push(format!("rm plist: {e}"));
        }
    }
    if let Some(log) = &spec.log {
        if log.starts_with("/usr/share") || log.starts_with("/Library/Apple") {
            let _ = fs::remove_file(log); // only exists if launchd managed to create it
        }
    }
    let fx = row.join("fx");
    let (ok, out) = fixtures::run("chflags", &["-R", "nouchg,noschg,nouappnd,nosappnd", &fx.to_string_lossy()]);
    if !ok {
        errs.push(format!("chflags: {out}"));
    }
    let mounts = fixtures::run("mount", &[]).1;
    let row_s = row.to_string_lossy();
    let private = format!("/private{row_s}");
    if mounts.lines().any(|l| l.contains(&*row_s) || l.contains(&private)) {
        errs.push("a mount is under the row tree; not deleting".into());
        return errs;
    }
    let (ok, out) = fixtures::run("rm", &["-rf", "-x", &row_s]);
    if !ok {
        errs.push(format!("rm: {out}"));
    }
    errs
}

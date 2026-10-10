//! One launchd job end to end: plist, bootstrap, `kickstart -p`, kqueue attach, verdict, bootout.
//!
//! Verdicts: `ran` (a `ready pid=P` line), `refused` (exited without one, status 78 `EX_CONFIG`),
//! `bootstrap-refused`, `spawn-refused`, anything else `inconclusive` plus an anomaly. Never
//! retried. Every wait is a kqueue event.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

use super::common::{cmd, Ctx, Res};
use crate::lifecycle::{drain, esc, field, make_fifo, open_raw, print_job, Fifo, Kq};
use crate::sys;

pub struct JobSpec<'a> {
    pub suffix: String,
    pub user: String,
    pub cwd: Option<PathBuf>,
    pub log: Option<PathBuf>,
    pub plist_dir: PathBuf,
    /// `(bits, path)` pairs the sentinel `access()`es as the account.
    pub access: Vec<(i32, PathBuf)>,
    /// Runs once the ready line is in and the job is still alive and loaded.
    pub on_ready: Option<&'a dyn Fn() -> Value>,
    /// After the job is released and gone, `kickstart` it again and record that too.
    pub second: bool,
}

impl<'a> JobSpec<'a> {
    pub fn new(suffix: &str, user: &str, plist_dir: &Path) -> JobSpec<'a> {
        JobSpec {
            suffix: suffix.into(),
            user: user.into(),
            cwd: None,
            log: None,
            plist_dir: plist_dir.into(),
            access: vec![],
            on_ready: None,
            second: false,
        }
    }
}

#[derive(Default)]
pub struct Run {
    pub verdict: String,
    pub exit_source: String,
    pub first_print: Option<Value>,
    pub ready: Option<String>,
    pub ready_parsed: Value,
    pub status_raw: String,
    pub exit: Option<String>,
    pub print_loaded: Option<Value>,
    pub on_ready: Option<Value>,
    pub kickstart: Value,
    pub anomalies: Vec<String>,
}

pub struct Outcome {
    pub verdict: String,
    pub exit_source: String,
    pub first_print: Option<Value>,
    pub first: Option<Run>,
    pub again: Option<Run>,
    pub bootstrap: Value,
    pub plist: String,
    pub anomalies: Vec<String>,
}

impl Outcome {
    pub fn detail(&self) -> Value {
        json!({
            "bootstrap": self.bootstrap, "plist": self.plist,
            "first": self.first.as_ref().map(run_json), "second": self.again.as_ref().map(run_json),
        })
    }

    pub fn ready(&self) -> Option<&Value> {
        self.first
            .as_ref()
            .filter(|r| r.ready.is_some())
            .map(|r| &r.ready_parsed)
    }

    /// Copies verdict, exit source, first print and anomalies into a result.
    pub fn apply(&self, res: &mut Res) {
        res.verdict = self.verdict.clone();
        res.exit_source = self.exit_source.clone();
        res.first_print = self.first_print.clone();
        res.anomalies.extend(self.anomalies.iter().cloned());
    }
}

fn run_json(r: &Run) -> Value {
    json!({
        "verdict": r.verdict, "exit_source": r.exit_source, "first_print": r.first_print,
        "ready": r.ready, "ready_parsed": r.ready_parsed, "status_raw": r.status_raw, "exit": r.exit,
        "print_loaded": r.print_loaded, "on_ready": r.on_ready, "kickstart": r.kickstart,
        "anomalies": r.anomalies,
    })
}

/// goetia's `restart: never` shape and key order (`generate.rs`).
pub fn plist(label: &str, args: &[String], cwd: Option<&Path>, user: &str, log: Option<&Path>) -> String {
    let mut d = format!(
        "<key>Label</key><string>{}</string>\n<key>ProgramArguments</key><array>",
        esc(label)
    );
    for a in args {
        d += &format!("<string>{}</string>", esc(a));
    }
    d += "</array>\n";
    if let Some(c) = cwd {
        d += &format!(
            "<key>WorkingDirectory</key><string>{}</string>\n",
            esc(&c.to_string_lossy())
        );
    }
    d += &format!("<key>UserName</key><string>{}</string>\n", esc(user));
    if let Some(l) = log {
        let l = esc(&l.to_string_lossy());
        d += &format!(
            "<key>StandardOutPath</key><string>{l}</string>\n<key>StandardErrorPath</key><string>{l}</string>\n"
        );
    }
    wrap(&d)
}

pub fn wrap(dict_body: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n{dict_body}</dict>\n</plist>\n"
    )
}

/// The smallest valid plist: a label and `/usr/bin/true`.
pub fn minimal_plist(label: &str) -> String {
    wrap(&format!(
        "<key>Label</key><string>{}</string>\n<key>ProgramArguments</key><array><string>/usr/bin/true</string></array>\n",
        esc(label)
    ))
}

pub fn write_plist(path: &Path, body: &[u8]) -> Result<(), String> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    f.write_all(body)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    std::os::unix::fs::chown(path, Some(0), Some(0)).map_err(|e| format!("chown {}: {e}", path.display()))
}

/// `last exit code = 78: EX_CONFIG` (or the older `last exit status = 19968`) as a code.
pub fn exit_code_from_print(text: &str) -> Option<i32> {
    if let Some(v) = field(text, "last exit code") {
        return v.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok();
    }
    let n: i32 = field(text, "last exit status")?.trim().parse().ok()?;
    Some(if n > 255 { n >> 8 } else { n })
}

pub fn print_summary(text: &str) -> Value {
    json!({
        "pid_line": super::common::text_has_pid_line(text),
        "last_exit_code": field(text, "last exit code").or_else(|| field(text, "last exit status")),
        "state": field(text, "state"),
        "path": field(text, "path"),
    })
}

/// Parses the sentinel's ready line into JSON.
pub fn parse_ready(line: &str) -> Value {
    let (head, cwd) = line.split_once(" cwd=").unwrap_or((line, ""));
    let mut out = json!({ "cwd": cwd });
    let mut held = vec![];
    for tok in head.split_whitespace().skip(1) {
        let Some((k, v)) = tok.split_once('=') else { continue };
        if let Some(n) = k.strip_prefix("fd").and_then(|n| n.parse::<u32>().ok()) {
            if v.starts_with("err:") {
                out[k] = json!({ "closed": v });
                continue;
            }
            let p: Vec<&str> = v.split(',').collect();
            let mut d = json!({ "fd": n, "getfd": p.first(), "getfl": p.get(1) });
            if p.len() == 6 {
                d["dev"] = json!(p[2]);
                d["ino"] = json!(p[3]);
                d["mode"] = json!(p[4]);
                d["rdev"] = json!(p[5]);
            } else if p.len() == 3 {
                d["staterr"] = json!(p[2]);
            }
            held.push(n);
            out[k] = d;
        } else {
            out[k] = json!(v);
        }
    }
    out["held"] = json!(held);
    out
}

struct Sync {
    status: Fifo,
    ctrl: Fifo,
    ctrl_path: PathBuf,
}

/// Bootstraps, runs the sentinel once (twice with `second`), boots it out, removes the plist.
pub fn launch(ctx: &Ctx, spec: &JobSpec) -> Outcome {
    let label = ctx.label(&spec.suffix);
    let base = ctx.base();
    let sync_dir = base.join(format!("sync-{}", spec.suffix));
    let plist_path = spec.plist_dir.join(format!("{label}.plist"));
    let mut out = Outcome {
        verdict: "inconclusive".into(),
        exit_source: "n/a".into(),
        first_print: None,
        first: None,
        again: None,
        bootstrap: Value::Null,
        plist: String::new(),
        anomalies: vec![],
    };
    ctx.state("dir", &sync_dir.to_string_lossy());
    if let Err(e) = super::common::mkdir_mode(&sync_dir, 0, 0, 0o755) {
        out.anomalies.push(e);
        return out;
    }
    let (sp, cp) = (sync_dir.join("STATUS"), sync_dir.join("CTRL"));
    let (Some(status), Some(ctrl)) = (make_fifo(&sp, &mut out.anomalies), make_fifo(&cp, &mut out.anomalies)) else {
        return out;
    };
    let mut sync = Sync {
        status,
        ctrl,
        ctrl_path: cp.clone(),
    };

    let mut args = vec![
        base.join("bin/job").to_string_lossy().into_owned(),
        sp.to_string_lossy().into_owned(),
        cp.to_string_lossy().into_owned(),
        format!("N1-{}-{}", ctx.run, spec.suffix),
        format!("N2-{}-{}", ctx.run, spec.suffix),
    ];
    for (bits, p) in &spec.access {
        args.push(bits.to_string());
        args.push(p.to_string_lossy().into_owned());
    }
    let body = plist(&label, &args, spec.cwd.as_deref(), &spec.user, spec.log.as_deref());
    out.plist = body.clone();
    ctx.state("label", &label);
    ctx.state("plist", &plist_path.to_string_lossy());
    if let Err(e) = write_plist(&plist_path, body.as_bytes()) {
        out.anomalies.push(e);
        return out;
    }
    ctx.phase(&spec.suffix, "bootstrap-started");
    let boot = cmd("launchctl", &["bootstrap", "system", &plist_path.to_string_lossy()]);
    ctx.phase(&spec.suffix, &format!("bootstrap-returned code={:?}", boot.code));
    out.bootstrap = boot.to_json();
    if !boot.ok() {
        out.verdict = "bootstrap-refused".into();
    } else {
        let ph = |w: &str| ctx.phase(&spec.suffix, w);
        let first = watch(&label, &mut sync, spec.on_ready, &ph);
        out.verdict = first.verdict.clone();
        out.exit_source = first.exit_source.clone();
        out.first_print = first.first_print.clone();
        out.anomalies.extend(first.anomalies.iter().cloned());
        out.first = Some(first);
        if spec.second {
            let ph2 = |w: &str| ctx.phase(&spec.suffix, &format!("second: {w}"));
            let again = watch(&label, &mut sync, None, &ph2);
            out.anomalies
                .extend(again.anomalies.iter().map(|a| format!("second launch: {a}")));
            out.again = Some(again);
        }
    }
    drop(sync);
    ctx.phase(&spec.suffix, &format!("bootout verdict={}", out.verdict));
    let bo = cmd("launchctl", &["bootout", &format!("system/{label}")]);
    let gone = bo.both().contains("No such process")
        || bo.both().contains("Could not find service")
        || bo.both().contains("No such file");
    if !bo.ok() && !gone && out.verdict != "bootstrap-refused" {
        out.anomalies.push(format!("bootout {label}: {}", bo.both().trim()));
    }
    if let Err(e) = fs::remove_file(&plist_path) {
        out.anomalies.push(format!("rm plist: {e}"));
    }
    out
}

/// `kickstart -p`, attach, wait for the ready line and the exit. Never retried.
fn watch(label: &str, sync: &mut Sync, on_ready: Option<&dyn Fn() -> Value>, phase: &dyn Fn(&str)) -> Run {
    let mut run = Run {
        exit_source: "n/a".into(),
        ready_parsed: Value::Null,
        kickstart: Value::Null,
        ..Default::default()
    };
    if sync.ctrl.w.is_none() {
        sync.ctrl.w = open_raw(&sync.ctrl_path, libc::O_WRONLY).ok();
        if sync.ctrl.w.is_none() {
            run.anomalies.push("reopen CTRL writer".into());
        }
    }
    phase("kickstart-started");
    let mut ks = Command::new("launchctl")
        .args(["kickstart", "-p", &format!("system/{label}")])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kickstart");
    let st = wait_own_child(&mut ks);
    let (mut so, mut se) = (String::new(), String::new());
    ks.stdout.take().unwrap().read_to_string(&mut so).ok();
    ks.stderr.take().unwrap().read_to_string(&mut se).ok();
    run.kickstart = json!({ "status": format!("{st:?}"), "stdout": so, "stderr": se });
    phase(&format!("kickstart-returned stdout={so:?}"));
    let Some(pid) = so.strip_suffix('\n').and_then(|d| d.parse::<i32>().ok()) else {
        run.verdict = "spawn-refused".into();
        run.first_print = Some(print_summary(&print_job(label)));
        return run;
    };

    let kq = Kq::new();
    let proc_r = kq.add(pid as usize, libc::EVFILT_PROC, libc::NOTE_EXIT | libc::NOTE_EXITSTATUS);
    let read_r = kq.add(sync.status.r.as_raw_fd() as usize, libc::EVFILT_READ, 0);
    if read_r != 0 {
        run.anomalies.push(format!("EVFILT_READ receipt {read_r}"));
    }
    let mut buf = Vec::new();
    let mut exit: Option<i32> = None;
    let mut printed: Option<String> = None;
    let mut waiting = false;
    if proc_r == libc::ESRCH as i64 {
        printed = Some(print_job(label));
    } else if proc_r != 0 {
        run.anomalies.push(format!("EVFILT_PROC receipt {proc_r}"));
        printed = Some(print_job(label));
    } else {
        let p = print_job(label);
        run.first_print = Some(print_summary(&p));
        match field(&p, "pid").map(str::trim) {
            Some(x) if x == pid.to_string() => waiting = true,
            _ => {
                // Ended before the print: whatever the queue holds now is all there will be.
                if let Some(evs) = kq.wait(Some(Duration::ZERO)) {
                    exit = evs
                        .iter()
                        .find(|e| e.filter == libc::EVFILT_PROC && e.fflags & libc::NOTE_EXIT != 0)
                        .map(|e| e.data as i32);
                }
                printed = Some(p);
            }
        }
    }
    phase(&format!("attached receipt={proc_r} waiting={waiting}"));
    if waiting {
        loop {
            let evs = kq.wait(None).unwrap_or_default();
            let got_exit = evs
                .iter()
                .find(|e| e.filter == libc::EVFILT_PROC && e.fflags & libc::NOTE_EXIT != 0)
                .map(|e| e.data as i32);
            drain(&sync.status.r, &mut buf);
            if run.ready.is_none() {
                if let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                    let line = String::from_utf8_lossy(&buf[..nl]).into_owned();
                    if !line.starts_with(&format!("ready pid={pid} ")) {
                        run.anomalies.push(format!("ready line from wrong pid: {line}"));
                    }
                    run.print_loaded = Some(json!({ "print": print_job(label) }));
                    run.on_ready = on_ready.map(|f| f());
                    run.ready = Some(line);
                    phase("ready; releasing");
                    sync.ctrl.w = None; // release: the job's read() sees EOF
                }
            }
            if let Some(st) = got_exit {
                exit = Some(st);
                break;
            }
        }
    } else {
        drain(&sync.status.r, &mut buf);
    }
    sync.ctrl.w = None;
    if run.ready.is_none() {
        if let Some(nl) = buf.iter().position(|b| *b == b'\n') {
            run.ready = Some(String::from_utf8_lossy(&buf[..nl]).into_owned());
            run.anomalies
                .push("ready line found only after the job was already gone".into());
        }
    }
    run.status_raw = String::from_utf8_lossy(&buf).into_owned();
    run.exit = exit.map(sys::decode_status);
    if let Some(r) = &run.ready {
        run.ready_parsed = parse_ready(r);
        run.verdict = "ran".into();
        run.exit_source = if exit.is_some() {
            "NOTE_EXITSTATUS".into()
        } else {
            "n/a".into()
        };
        if let Some(code) = exit.and_then(sys::exited_code) {
            if code != 0 {
                run.anomalies.push(format!("ran job exited {code}, want 0"));
            }
        }
        return run;
    }
    classify_refusal(&mut run, exit, printed);
    run
}

/// X1: the exit's source, and the first print when the attach could not give one.
fn classify_refusal(run: &mut Run, exit: Option<i32>, printed: Option<String>) {
    if let Some(st) = exit {
        run.exit_source = "NOTE_EXITSTATUS".into();
        if sys::exited_code(st) == Some(78) {
            run.verdict = "refused".into();
        } else {
            run.verdict = "inconclusive".into();
            run.anomalies.push(format!(
                "no ready line, exit {}, want exited:78",
                sys::decode_status(st)
            ));
        }
        return;
    }
    run.exit_source = "ESRCH+print".into();
    let p = printed.unwrap_or_else(|| "(no print)".into());
    if run.first_print.is_none() {
        run.first_print = Some(print_summary(&p));
    }
    let settled = !super::common::text_has_pid_line(&p) && exit_code_from_print(&p) == Some(78);
    if settled {
        run.verdict = "refused".into();
    } else {
        run.verdict = "inconclusive".into();
        run.anomalies
            .push(format!("bookkeeping not settled: first print {:?}", run.first_print));
    }
}

/// Wait for our own child with a kqueue event (the same register-then-`try_wait` order as before).
fn wait_own_child(child: &mut std::process::Child) -> std::process::ExitStatus {
    let kq = Kq::new();
    let r = kq.add(child.id() as usize, libc::EVFILT_PROC, libc::NOTE_EXIT);
    if let Ok(Some(st)) = child.try_wait() {
        return st;
    }
    if r == 0 {
        kq.wait(None);
    }
    child.wait().expect("kickstart wait")
}

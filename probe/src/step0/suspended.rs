//! The suspended-start lifecycle (pass 2, M2): `bootstrap`, then per launch `kickstart -s -p`, a
//! kqueue attach with `EV_RECEIPT`, `SIGCONT` only after a good receipt, and one wait. Never
//! retried; every wait is a kqueue event.

use std::fs;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use super::common::{cmd, mkdir_mode, Ctx};
use super::launch::{plist, print_summary, wait_own_child, write_plist, JobSpec, Sync};
use crate::lifecycle::{drain, field, make_fifo, open_raw, print_job, Kq};
use crate::sys;

/// A bootstrapped job that can be launched more than once under the same label.
pub struct Session {
    pub label: String,
    pub bootstrap: Value,
    pub bootstrapped: bool,
    plist_path: PathBuf,
    sync: Option<Sync>,
    pub anomalies: Vec<String>,
}

/// One launch.
pub struct Launch {
    /// `ran`, `refused` (exited 78, no ready line), `exit-other`, `esrch` (the attach found no
    /// process), `no-pid` (`kickstart -s -p` gave no pid), `attach-failed`.
    pub verdict: String,
    pub record: Value,
    pub anomalies: Vec<String>,
}

impl Session {
    /// Writes the plist and bootstraps it. A refused bootstrap is recorded, not an anomaly.
    pub fn open(ctx: &Ctx, spec: &JobSpec) -> Session {
        let label = ctx.label(&spec.suffix);
        let base = ctx.base();
        let sync_dir = base.join(format!("sync-{}", spec.suffix));
        let plist_path = spec.plist_dir.join(format!("{label}.plist"));
        let mut s = Session {
            label: label.clone(),
            bootstrap: Value::Null,
            bootstrapped: false,
            plist_path: plist_path.clone(),
            sync: None,
            anomalies: vec![],
        };
        ctx.state("dir", &sync_dir.to_string_lossy());
        if let Err(e) = mkdir_mode(&sync_dir, 0, 0, 0o755) {
            s.anomalies.push(e);
            return s;
        }
        let (sp, cp) = (sync_dir.join("STATUS"), sync_dir.join("CTRL"));
        let (Some(status), Some(ctrl)) = (make_fifo(&sp, &mut s.anomalies), make_fifo(&cp, &mut s.anomalies)) else {
            return s;
        };
        s.sync = Some(Sync {
            status,
            ctrl,
            ctrl_path: cp.clone(),
        });
        let args = vec![
            base.join("bin/job").to_string_lossy().into_owned(),
            sp.to_string_lossy().into_owned(),
            cp.to_string_lossy().into_owned(),
            format!("N1-{}-{}", ctx.run, spec.suffix),
            format!("N2-{}-{}", ctx.run, spec.suffix),
        ];
        let body = plist(&label, &args, spec.cwd.as_deref(), &spec.user, spec.log.as_deref());
        ctx.state("label", &label);
        ctx.state("plist", &plist_path.to_string_lossy());
        if let Err(e) = write_plist(&plist_path, body.as_bytes()) {
            s.anomalies.push(e);
            return s;
        }
        let boot = cmd("launchctl", &["bootstrap", "system", &plist_path.to_string_lossy()]);
        s.bootstrapped = boot.ok();
        s.bootstrap = boot.to_json();
        s
    }

    /// Boots the job out and removes the plist.
    pub fn close(mut self) -> Vec<String> {
        self.sync = None;
        let mut an = std::mem::take(&mut self.anomalies);
        let bo = cmd("launchctl", &["bootout", &format!("system/{}", self.label)]);
        let b = bo.both();
        let gone = b.contains("No such process") || b.contains("Could not find service") || b.contains("No such file");
        if !bo.ok() && !gone && self.bootstrapped {
            an.push(format!("bootout {}: {}", self.label, b.trim()));
        }
        if let Err(e) = fs::remove_file(&self.plist_path) {
            an.push(format!("rm plist: {e}"));
        }
        an
    }

    /// One `kickstart -s -p`, attach, `SIGCONT` after a good receipt, wait for the first event.
    /// `progress` gets the record so far, just before the wait that may not end.
    pub fn launch(&mut self, phase: &dyn Fn(&str), progress: &dyn Fn(&Value)) -> Launch {
        let label = self.label.clone();
        let mut l = Launch {
            verdict: "inconclusive".into(),
            record: json!({}),
            anomalies: vec![],
        };
        if !self.bootstrapped {
            l.verdict = "bootstrap-refused".into();
            l.record["bootstrap"] = self.bootstrap.clone();
            return l;
        }
        let Some(sync) = self.sync.as_mut() else {
            l.anomalies.push("no sync fifos".into());
            return l;
        };
        if sync.ctrl.w.is_none() {
            sync.ctrl.w = open_raw(&sync.ctrl_path, libc::O_WRONLY).ok();
            if sync.ctrl.w.is_none() {
                l.anomalies.push("reopen CTRL writer".into());
            }
        }
        phase("kickstart-s-started");
        let mut ks = Command::new("launchctl")
            .args(["kickstart", "-s", "-p", &format!("system/{label}")])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kickstart");
        let st = wait_own_child(&mut ks);
        let (mut so, mut se) = (String::new(), String::new());
        ks.stdout.take().unwrap().read_to_string(&mut so).ok();
        ks.stderr.take().unwrap().read_to_string(&mut se).ok();
        l.record["kickstart"] = json!({ "status": format!("{st:?}"), "stdout": so, "stderr": se });
        phase(&format!("kickstart-s-returned stdout={so:?}"));
        // Pass 1's `-p` printed the bare pid. If `-s` changes the format, the last number on the
        // last line is taken and the parse is recorded; the raw stdout is in the record either way.
        let strict = so.strip_suffix('\n').and_then(|d| d.parse::<i32>().ok());
        let lenient = || {
            let tok = so
                .lines()
                .last()?
                .rsplit(|c: char| !c.is_ascii_digit())
                .find(|t| !t.is_empty())?;
            tok.parse::<i32>().ok()
        };
        l.record["pid_parse"] = json!(if strict.is_some() { "strict" } else { "lenient" });
        let Some(pid) = strict.or_else(lenient) else {
            l.verdict = "no-pid".into();
            l.record["print_after"] = print_summary(&print_job(&label));
            return l;
        };
        l.record["pid"] = json!(pid);
        // Recorded only, never gating.
        l.record["ps_state"] = json!(cmd("ps", &["-o", "state=", "-p", &pid.to_string()]).stdout.trim());

        let kq = Kq::new();
        let proc_r = kq.add(pid as usize, libc::EVFILT_PROC, libc::NOTE_EXIT | libc::NOTE_EXITSTATUS);
        let read_r = kq.add(sync.status.r.as_raw_fd() as usize, libc::EVFILT_READ, 0);
        l.record["receipt_proc"] = json!(proc_r);
        l.record["receipt_read"] = json!(read_r);
        if read_r != 0 {
            l.anomalies.push(format!("EVFILT_READ receipt {read_r}"));
        }
        // Before SIGCONT: the print a stale `last exit code` would show up in.
        let suspended_print = print_job(&label);
        l.record["print_suspended"] = print_summary(&suspended_print);
        l.record["print_suspended"]["pid_matches"] =
            json!(field(&suspended_print, "pid").map(str::trim) == Some(&pid.to_string()));
        let mut buf = Vec::new();
        if proc_r != 0 {
            // Never resumed: without a receipt there is nothing to wait on.
            drain(&sync.status.r, &mut buf);
            l.record["status_raw"] = json!(String::from_utf8_lossy(&buf));
            l.record["resumed"] = json!(false);
            l.record["print_after_exit"] = json!({ "pid": pid, "print": print_summary(&print_job(&label)) });
            if proc_r == libc::ESRCH as i64 {
                l.verdict = "esrch".into();
            } else {
                l.verdict = "attach-failed".into();
                l.anomalies.push(format!("EVFILT_PROC receipt {proc_r}"));
            }
            sync.ctrl.w = None;
            return l;
        }
        let rc = unsafe { libc::kill(pid, libc::SIGCONT) };
        l.record["sigcont"] = json!(if rc == 0 { 0 } else { sys::errno() });
        l.record["resumed"] = json!(rc == 0);
        if rc != 0 {
            l.anomalies.push(format!("SIGCONT: errno {}", sys::errno()));
        }
        phase("resumed; waiting for the first event");
        progress(&l.record);

        let mut ready: Option<String> = None;
        let exit: Option<i32> = loop {
            let evs = kq.wait(None).unwrap_or_default();
            let got_exit = evs
                .iter()
                .find(|e| e.filter == libc::EVFILT_PROC && e.fflags & libc::NOTE_EXIT != 0)
                .map(|e| e.data as i32);
            drain(&sync.status.r, &mut buf);
            if ready.is_none() {
                if let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                    let line = String::from_utf8_lossy(&buf[..nl]).into_owned();
                    if !line.starts_with(&format!("ready pid={pid} ")) {
                        l.anomalies.push(format!("ready line from wrong pid: {line}"));
                    }
                    // Job live, CTRL held.
                    l.record["print_ready"] = print_summary(&print_job(&label));
                    ready = Some(line);
                    phase("ready; releasing");
                    sync.ctrl.w = None;
                }
            }
            if got_exit.is_some() {
                break got_exit;
            }
        };
        sync.ctrl.w = None;
        l.record["status_raw"] = json!(String::from_utf8_lossy(&buf));
        l.record["exit"] = json!(exit.map(sys::decode_status));
        l.record["ready"] = json!(ready);
        l.record["print_after_exit"] = json!({ "pid": pid, "print": print_summary(&print_job(&label)) });
        l.verdict = if ready.is_some() {
            if exit.and_then(sys::exited_code) != Some(0) {
                l.anomalies.push(format!(
                    "ran job exited {:?}, want exited:0",
                    exit.map(sys::decode_status)
                ));
            }
            "ran".into()
        } else if exit.and_then(sys::exited_code) == Some(78) {
            "refused".into()
        } else {
            "exit-other".into()
        };
        l
    }
}

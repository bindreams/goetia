//! Shared harness: guards, runner info, the state file, the result writer, the anomaly rule,
//! fresh-id accounts and small command helpers.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use crate::sys::{self, Pw};

/// Exit code for a refused guard.
pub const EXIT_GUARD: i32 = 2;

pub struct Ctx {
    pub run: String,
    pub job: String,
    pub results: PathBuf,
    pub os: Value,
    pub anomalies: usize,
}

/// `GITHUB_ACTIONS=true`, `RUNNER_OS=macOS`, `RUNNER_ENVIRONMENT=github-hosted`: anything else is
/// not a throwaway runner.
pub fn guard_failure() -> Option<String> {
    for (k, want) in [
        ("GITHUB_ACTIONS", "true"),
        ("RUNNER_OS", "macOS"),
        ("RUNNER_ENVIRONMENT", "github-hosted"),
    ] {
        let got = std::env::var(k).ok();
        if got.as_deref() != Some(want) {
            return Some(format!("guard: {k} is {got:?}, want {want:?}"));
        }
    }
    None
}

pub fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.into())
}

impl Ctx {
    /// Checks the guards before touching anything.
    pub fn init() -> Result<Ctx, i32> {
        if let Some(why) = guard_failure() {
            eprintln!("refusing to run: {why}");
            return Err(EXIT_GUARD);
        }
        let run = std::env::var("PROBE_RUN").map_err(|_| {
            eprintln!("refusing to run: PROBE_RUN unset");
            EXIT_GUARD
        })?;
        let job = env_or("PROBE_JOB", "unknown");
        let results = PathBuf::from("results").join(&job);
        fs::create_dir_all(&results).expect("results dir");
        let sw = sh("sw_vers");
        let os = json!({
            "image": env_or("PROBE_IMAGE", "unknown"),
            "sw_vers": sw.replace('\n', "; "),
            "build": sh("sw_vers -buildVersion").trim(),
            "csrutil": sh("csrutil status").trim(),
        });
        Ok(Ctx {
            run,
            job,
            results,
            os,
            anomalies: 0,
        })
    }

    /// `/tmp/goetia-probe-<run>`: the staged root-owned `bin/` and per-step scratch.
    pub fn base(&self) -> PathBuf {
        PathBuf::from(format!("/tmp/goetia-probe-{}", self.run))
    }

    pub fn label(&self, suffix: &str) -> String {
        format!("com.goetia.probe.{}.{suffix}", self.run)
    }

    pub fn state_path(&self) -> PathBuf {
        PathBuf::from(env_or("RUNNER_TEMP", "/tmp")).join(format!("goetia-probe-state-{}", self.job))
    }

    /// Appends one `key=value` line, and syncs it, before the caller acts.
    pub fn state(&self, key: &str, value: &str) {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.state_path())
            .expect("state file");
        writeln!(f, "{key}={value}").expect("state write");
        f.sync_all().expect("state sync");
    }

    pub fn emit(&mut self, mut r: Res) {
        self.anomalies += r.anomalies.len();
        let doc = r.to_json(&self.os);
        let text = doc.to_string();
        let file = self.results.join(format!("{}.json", r.id));
        fs::write(&file, &text).unwrap_or_else(|e| panic!("write {}: {e}", file.display()));
        println!("RESULT {text}");
        r.anomalies.clear();
    }

    /// A step is red only on an anomaly.
    pub fn finish(self) -> i32 {
        if self.anomalies > 0 {
            eprintln!("{} anomaly(ies) in this step", self.anomalies);
            1
        } else {
            0
        }
    }

    pub fn note(&mut self, what: &str) {
        self.anomalies += 1;
        eprintln!("anomaly: {what}");
    }
}

pub struct Res {
    pub id: String,
    pub group: &'static str,
    pub question: Vec<&'static str>,
    pub expected: Value,
    pub observed: Value,
    pub verdict: String,
    pub differs: Option<bool>,
    pub exit_source: String,
    pub first_print: Option<Value>,
    pub anomalies: Vec<String>,
}

impl Res {
    pub fn new(id: &str, group: &'static str, question: &[&'static str]) -> Res {
        Res {
            id: id.into(),
            group,
            question: question.to_vec(),
            expected: Value::Null,
            observed: json!({}),
            verdict: "value".into(),
            differs: None,
            exit_source: "n/a".into(),
            first_print: None,
            anomalies: vec![],
        }
    }

    pub fn expect(mut self, v: Value) -> Res {
        self.expected = v;
        self
    }

    pub fn verdict(mut self, v: &str) -> Res {
        self.verdict = v.into();
        self
    }

    pub fn anomaly(&mut self, a: impl Into<String>) {
        self.anomalies.push(a.into());
    }

    fn to_json(&self, os: &Value) -> Value {
        json!({
            "id": self.id, "group": self.group, "os": os, "question": self.question,
            "expected": self.expected, "observed": self.observed, "verdict": self.verdict,
            "differs": self.differs, "exit_source": self.exit_source, "first_print": self.first_print,
            "anomalies": self.anomalies,
        })
    }
}

// Commands ----------------------------------------------------------------------------------------

/// `sh -c` output (stdout then stderr), never failing.
pub fn sh(cmd: &str) -> String {
    match Command::new("/bin/sh").args(["-c", cmd]).output() {
        Err(e) => format!("spawn sh: {e}"),
        Ok(o) => format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
    }
}

pub struct Out {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    pub fn both(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }

    pub fn to_json(&self) -> Value {
        json!({ "code": self.code, "stdout": self.stdout, "stderr": self.stderr })
    }
}

pub fn cmd(program: &str, args: &[&str]) -> Out {
    match Command::new(program).args(args).output() {
        Err(e) => Out {
            code: None,
            stdout: String::new(),
            stderr: format!("spawn {program}: {e}"),
        },
        Ok(o) => Out {
            code: o.status.code(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
    }
}

/// Runs a command as `user` through `sudo -n -u` (sudo does the initgroups).
pub fn as_user(user: &str, program: &str, args: &[&str]) -> Out {
    let mut a = vec!["-n", "-u", user, program];
    a.extend_from_slice(args);
    cmd("/usr/bin/sudo", &a)
}

/// A command that must succeed; a failure is recorded in `anomalies`.
pub fn must(anomalies: &mut Vec<String>, program: &str, args: &[&str]) -> Out {
    let o = cmd(program, args);
    if !o.ok() {
        anomalies.push(format!("{program} {args:?}: {:?} {}", o.code, o.both().trim()));
    }
    o
}

// Filesystem ---------------------------------------------------------------------------------------

pub fn mkdir_mode(p: &Path, uid: u32, gid: u32, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(p).map_err(|e| format!("mkdir {}: {e}", p.display()))?;
    std::os::unix::fs::chown(p, Some(uid), Some(gid)).map_err(|e| format!("chown {}: {e}", p.display()))?;
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).map_err(|e| format!("chmod {}: {e}", p.display()))
}

/// Creates `p` root:wheel 0755 and records it in the state file before creating.
pub fn scratch_dir(ctx: &Ctx, p: &Path) -> Result<(), String> {
    ctx.state("dir", &p.to_string_lossy());
    mkdir_mode(p, 0, 0, 0o755)
}

pub fn write_file(p: &Path, uid: u32, gid: u32, mode: u32, content: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::write(p, content).map_err(|e| format!("write {}: {e}", p.display()))?;
    std::os::unix::fs::chown(p, Some(uid), Some(gid)).map_err(|e| format!("chown {}: {e}", p.display()))?;
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).map_err(|e| format!("chmod {}: {e}", p.display()))
}

/// `statfs` of a path as JSON, or its errno.
pub fn statfs_json(path: &Path) -> Value {
    let c = sys::cpath(path);
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return json!({ "errno": sys::errno() });
    }
    statfs_value(&s)
}

pub fn cstr_field(a: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = a.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

pub fn statfs_value(s: &libc::statfs) -> Value {
    // `fsid_t` is two `i32`s with a private field in the `libc` crate.
    let fsid: [i32; 2] = unsafe { std::mem::transmute(s.f_fsid) };
    json!({
        "fsid": fsid,
        "fstypename": cstr_field(&s.f_fstypename),
        "mntonname": cstr_field(&s.f_mntonname),
        "mntfromname": cstr_field(&s.f_mntfromname),
        "owner": s.f_owner,
        "flags": format!("{:#x}", s.f_flags),
        "flags_named": flag_names(s.f_flags),
    })
}

pub const MNT_RDONLY: u32 = 0x1;
pub const MNT_NOSUID: u32 = 0x8;
pub const MNT_NODEV: u32 = 0x10;
pub const MNT_REMOVABLE: u32 = 0x200;
pub const MNT_LOCAL: u32 = 0x1000;
pub const MNT_DONTBROWSE: u32 = 0x100000;
pub const MNT_IGNORE_OWNERSHIP: u32 = 0x200000;

pub fn flag_names(f: u32) -> Vec<&'static str> {
    [
        (MNT_RDONLY, "RDONLY"),
        (MNT_NOSUID, "NOSUID"),
        (MNT_NODEV, "NODEV"),
        (MNT_REMOVABLE, "REMOVABLE"),
        (MNT_LOCAL, "LOCAL"),
        (MNT_DONTBROWSE, "DONTBROWSE"),
        (MNT_IGNORE_OWNERSHIP, "IGNORE_OWNERSHIP"),
    ]
    .into_iter()
    .filter(|(b, _)| f & b != 0)
    .map(|(_, n)| n)
    .collect()
}

pub fn st_dev(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(p).ok().map(|m| m.dev())
}

// Accounts -----------------------------------------------------------------------------------------

fn dscl_ids(kind: &str, attr: &str) -> Vec<u32> {
    cmd("dscl", &[".", "-list", kind, attr])
        .stdout
        .lines()
        .filter_map(|l| l.split_whitespace().last()?.parse().ok())
        .collect()
}

/// A fresh uid and gid: above every id in use, so a name is never reused for another id.
fn fresh_id() -> u32 {
    let used = dscl_ids("/Users", "UniqueID")
        .into_iter()
        .chain(dscl_ids("/Groups", "PrimaryGroupID"));
    used.max().unwrap_or(0).max(699_999) + 1
}

/// A local user and a same-named primary group, with a fresh id. Recorded in the state file
/// before each creation step. `home` is created (root-owned tree handed to the user).
pub fn create_account(ctx: &Ctx, tag: &str, home: Option<&Path>, anomalies: &mut Vec<String>) -> Option<Pw> {
    let name = format!("_g{tag}-{}", ctx.run);
    let id = fresh_id();
    let (g, u) = (format!("/Groups/{name}"), format!("/Users/{name}"));
    ctx.state("group", &name);
    must(anomalies, "dscl", &[".", "-create", &g]);
    must(
        anomalies,
        "dscl",
        &[".", "-create", &g, "PrimaryGroupID", &id.to_string()],
    );
    ctx.state("user", &name);
    must(anomalies, "dscl", &[".", "-create", &u]);
    let home_s = home.map_or("/var/empty".to_string(), |h| h.to_string_lossy().into_owned());
    for (k, v) in [
        ("UniqueID", id.to_string()),
        ("PrimaryGroupID", id.to_string()),
        ("UserShell", "/usr/bin/false".into()),
        ("NFSHomeDirectory", home_s),
        ("RealName", name.clone()),
    ] {
        must(anomalies, "dscl", &[".", "-create", &u, k, &v]);
    }
    if let Some(h) = home {
        ctx.state("dir", &h.to_string_lossy());
        if let Err(e) = mkdir_mode(h, id, id, 0o755) {
            anomalies.push(e);
        }
    }
    let pw = sys::getpwnam(&name);
    match &pw {
        Some(p) if p.uid == id && p.gid == id => {}
        other => anomalies.push(format!(
            "account {name}: resolves to {:?}, want {id}",
            other.as_ref().map(|p| (p.uid, p.gid))
        )),
    }
    pw
}

pub fn pw_json(p: &Pw) -> Value {
    json!({ "name": p.name, "uid": p.uid, "gid": p.gid })
}

pub fn text_has_pid_line(text: &str) -> bool {
    text.lines().any(|l| l.trim_start().starts_with("pid = "))
}

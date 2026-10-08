//! One fixture per row id, each with exactly one bad variable under `<row>/fx`.

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use serde_json::{json, Value};

use crate::candidates::Target;
use crate::sys::{self, Pw, A, R, W, X};

pub const ALL: &[&str] = &[
    "F1c", "F2c", "F25c", "F5l", "F19a", "F6l", "F7l", "F12l", "F26l", "F21b", "F22l", "F23l", "F24l", "F28l",
    "F29l", "F14g", "F18g", "F20g", "F16l", "F17l",
];

#[derive(Clone, Copy, Debug, Serialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Cwd,
    LogCreate,
    LogExisting,
    LogOther,
    Groups,
    /// F19a: cwd and log-create at once (the reverse-tunnel shape).
    CwdAndLog,
}

pub struct Spec {
    pub kind: Kind,
    /// No `ready` line here is an anomaly, not data.
    pub must_run: bool,
    pub acct: Pw,
    pub cwd: Option<PathBuf>,
    pub log: Option<PathBuf>,
    pub job_needs: Vec<Target>,
    pub targets: Vec<Target>,
    pub run_static: bool,
    pub hold_log_reader: bool,
    pub bounded_wait: bool,
    pub facts: Value,
    pub unavailable: Option<String>,
    pub users: Vec<String>,
    pub groups: Vec<String>,
    pub anomalies: Vec<String>,
}

pub struct Ctx {
    pub fx: PathBuf,
    pub anomalies: Vec<String>,
}

impl Ctx {
    fn mkdir(&mut self, rel: &str, uid: u32, gid: u32, mode: u32) -> PathBuf {
        let p = self.fx.join(rel);
        fs::create_dir(&p).unwrap_or_else(|e| panic!("mkdir {}: {e}", p.display()));
        own(&p, uid, gid, mode);
        self.check(&p, 'd', uid, mode);
        p
    }

    fn mkfile(&mut self, rel: &str, uid: u32, gid: u32, mode: u32, content: &[u8]) -> PathBuf {
        let p = self.fx.join(rel);
        fs::write(&p, content).unwrap_or_else(|e| panic!("write {}: {e}", p.display()));
        own(&p, uid, gid, mode);
        self.check(&p, 'f', uid, mode);
        p
    }

    /// Fixture self-check: the path is what the table says.
    fn check(&mut self, p: &Path, ty: char, uid: u32, mode: u32) {
        use std::os::unix::fs::MetadataExt;
        match fs::symlink_metadata(p) {
            Err(e) => self.anomalies.push(format!("fixture {}: {e}", p.display())),
            Ok(m) => {
                let ok_ty = match ty {
                    'd' => m.is_dir(),
                    'f' => m.is_file(),
                    'p' => m.mode() & libc::S_IFMT as u32 == libc::S_IFIFO as u32,
                    _ => false,
                };
                if !ok_ty || m.uid() != uid || m.mode() & 0o7777 != mode {
                    self.anomalies.push(format!(
                        "fixture {}: type ok {ok_ty}, uid {} (want {uid}), mode {:o} (want {mode:o})",
                        p.display(),
                        m.uid(),
                        m.mode() & 0o7777
                    ));
                }
            }
        }
    }
}

fn own(p: &Path, uid: u32, gid: u32, mode: u32) {
    std::os::unix::fs::lchown(p, Some(uid), Some(gid)).unwrap_or_else(|e| panic!("chown {}: {e}", p.display()));
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap_or_else(|e| panic!("chmod {}: {e}", p.display()));
}

pub fn run(cmd: &str, args: &[&str]) -> (bool, String) {
    match Command::new(cmd).args(args).output() {
        Err(e) => (false, format!("spawn {cmd}: {e}")),
        Ok(o) => (
            o.status.success(),
            format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)),
        ),
    }
}

fn must(cmd: &str, args: &[&str], anomalies: &mut Vec<String>) -> String {
    let (ok, out) = run(cmd, args);
    if !ok {
        anomalies.push(format!("{cmd} {args:?} failed: {out}"));
    }
    out
}

fn t(need: &str, bits: i32, path: &Path) -> Target {
    Target {
        need: need.into(),
        bits,
        path: path.to_path_buf(),
    }
}

/// Every non-empty combination of R/W/A, each as one combined `access()` (H-2).
fn file_subsets(p: &Path) -> Vec<Target> {
    let mut v = Vec::new();
    for mask in 1..8 {
        let mut bits = 0;
        let mut name = String::new();
        for (bit, b, n) in [(1, R, 'R'), (2, W, 'W'), (4, A, 'A')] {
            if mask & bit != 0 {
                bits |= b;
                name.push(n);
            }
        }
        v.push(t(&name, bits, p));
    }
    v
}

pub fn nobody() -> Pw {
    sys::getpwnam("nobody").expect("nobody exists")
}

fn root() -> Pw {
    sys::getpwnam("root").expect("root exists")
}

/// A fresh, never-reused id must not exist yet.
fn assert_unused_id(kind: &str, id: u32, anomalies: &mut Vec<String>) {
    let attr = if kind == "Users" { "UniqueID" } else { "PrimaryGroupID" };
    let (_, out) = run("dscl", &[".", "-search", &format!("/{kind}"), attr, &id.to_string()]);
    if !out.trim().is_empty() {
        anomalies.push(format!("fresh {kind} id {id} already in use: {out}"));
    }
}

fn create_group(name: &str, gid: u32, members: &[&str], anomalies: &mut Vec<String>) {
    assert_unused_id("Groups", gid, anomalies);
    let g = format!("/Groups/{name}");
    must("dscl", &[".", "-create", &g], anomalies);
    must("dscl", &[".", "-create", &g, "PrimaryGroupID", &gid.to_string()], anomalies);
    must("dscl", &[".", "-create", &g, "RealName", name], anomalies);
    for m in members {
        must("dscl", &[".", "-append", &g, "GroupMembership", m], anomalies);
    }
}

fn create_user(name: &str, uid: u32, pgid: u32, anomalies: &mut Vec<String>) {
    assert_unused_id("Users", uid, anomalies);
    let u = format!("/Users/{name}");
    must("dscl", &[".", "-create", &u], anomalies);
    for (k, v) in [
        ("UniqueID", uid.to_string()),
        ("PrimaryGroupID", pgid.to_string()),
        ("UserShell", "/usr/bin/false".into()),
        ("NFSHomeDirectory", "/var/empty".into()),
        ("RealName", name.into()),
    ] {
        must("dscl", &[".", "-create", &u, k, &v], anomalies);
    }
}

fn is_member(user: &str, group: &str) -> (bool, String) {
    let (_, out) = run("dsmemberutil", &["checkmembership", "-U", user, "-G", group]);
    (out.contains("user is a member"), out.trim().to_string())
}

pub fn build(id: &str, run_id: &str, fx: &Path) -> Spec {
    let mut c = Ctx {
        fx: fx.to_path_buf(),
        anomalies: Vec::new(),
    };
    let nb = nobody();
    let (nu, ng) = (nb.uid, nb.gid);
    let mut s = Spec {
        kind: Kind::LogCreate,
        must_run: false,
        acct: nb,
        cwd: None,
        log: None,
        job_needs: vec![],
        targets: vec![],
        run_static: false,
        hold_log_reader: false,
        bounded_wait: false,
        facts: json!({}),
        unavailable: None,
        users: vec![],
        groups: vec![],
        anomalies: vec![],
    };
    match id {
        "F1c" | "F2c" => {
            let d = c.mkdir("d", 0, 0, if id == "F1c" { 0o700 } else { 0o755 });
            s.kind = Kind::Cwd;
            s.must_run = id == "F2c";
            s.targets = vec![t("X", X, &d)];
            s.cwd = Some(d);
        }
        "F25c" => {
            c.mkdir("a", 0, 0, 0o755);
            symlink(fx.join("nowhere"), fx.join("a/link")).expect("symlink");
            let cwd = fx.join("a/link/b");
            s.kind = Kind::Cwd;
            s.targets = vec![t("X", X, &cwd)];
            s.cwd = Some(cwd);
        }
        "F5l" => {
            let d = c.mkdir("d", 0, 0, 0o755);
            s.targets = vec![t("WX", W | X, &d)];
            s.log = Some(d.join("out.log"));
        }
        "F19a" => {
            c.mkdir("base", 0, 0, 0o755);
            let logs = c.mkdir("base/logs", nu, ng, 0o750);
            s.kind = Kind::CwdAndLog;
            s.must_run = true;
            s.targets = vec![t("X", X, &logs), t("WX", W | X, &logs)];
            s.cwd = Some(logs.clone());
            s.log = Some(logs.join("frpc.log"));
        }
        "F6l" | "F7l" | "F12l" | "F26l" | "F21b" => {
            let (duid, dgid, fuid, fgid, fmode) = match id {
                "F6l" => (nu, ng, 0, 0, 0o644),
                "F12l" => (0, 0, 0, 0, 0o600),
                _ => (0, 0, nu, ng, 0o644),
            };
            c.mkdir("d", duid, dgid, 0o755);
            let f = c.mkfile("d/out.log", fuid, fgid, fmode, b"pre\n");
            let fs_ = f.to_string_lossy().into_owned();
            match id {
                "F12l" => {
                    must("chmod", &["+a", "nobody allow append", &fs_], &mut c.anomalies);
                }
                "F26l" => {
                    must("chflags", &["uappnd", &fs_], &mut c.anomalies);
                }
                "F21b" => {
                    must("chflags", &["uchg", &fs_], &mut c.anomalies);
                }
                _ => {}
            }
            s.facts = json!({ "ls": run("ls", &["-leO", &fs_]).1 });
            s.kind = Kind::LogExisting;
            s.must_run = id == "F7l";
            s.targets = file_subsets(&f);
            s.log = Some(f);
        }
        "F22l" => {
            c.mkdir("d", nu, ng, 0o755);
            let f = c.mkdir("d/out.log", nu, ng, 0o755);
            s.kind = Kind::LogOther;
            s.log = Some(f); // excluded from candidate comparisons (L2): no targets
        }
        "F23l" => {
            let d = c.mkdir("d", 0, 0, 0o755);
            let tdir = c.mkdir("t", nu, ng, 0o755);
            symlink(tdir.join("target.log"), d.join("out.log")).expect("symlink");
            s.kind = Kind::LogOther;
            s.targets = vec![t("WX", W | X, &tdir)];
            s.log = Some(d.join("out.log"));
        }
        "F24l" => {
            s.kind = Kind::LogOther;
            s.must_run = true;
            s.targets = file_subsets(Path::new("/dev/null"));
            s.log = Some(PathBuf::from("/dev/null"));
        }
        "F28l" | "F29l" => {
            c.mkdir("d", 0, 0, 0o755);
            let f = fx.join("d/out.log");
            let cf = sys::cpath(&f);
            if unsafe { libc::mkfifo(cf.as_ptr(), 0o666) } != 0 {
                c.anomalies.push(format!("mkfifo: {}", sys::errno()));
            }
            own(&f, 0, 0, 0o666);
            c.check(&f, 'p', 0, 0o666);
            s.kind = Kind::LogOther;
            s.targets = file_subsets(&f);
            s.hold_log_reader = id == "F28l";
            s.bounded_wait = id == "F29l";
            s.log = Some(f);
        }
        "F14g" => {
            let g = c.mkdir("G", 0, 12, 0o770);
            group_row(&mut s, &g);
            s.facts = json!({ "group": "everyone", "gid": 12 });
        }
        "F18g" => {
            let user = "uprb18".to_string();
            let pg = 610_000u32;
            create_group("gprb18p", pg, &[], &mut c.anomalies);
            let names: Vec<String> = (1..=24).map(|i| format!("gprb18x{i:02}")).collect();
            for (i, n) in names.iter().enumerate() {
                create_group(n, pg + 1 + i as u32, &[&user], &mut c.anomalies);
            }
            create_user(&user, 610_100, pg, &mut c.anomalies);
            s.users.push(user.clone());
            s.groups.push("gprb18p".into());
            s.groups.extend(names.iter().cloned());
            let acct = sys::getpwnam(&user).expect("fresh user resolves");
            let full = sys::getgrouplist(&user, acct.gid, 128);
            let first16 = sys::getgrouplist(&user, acct.gid, 16);
            let chosen = names.iter().enumerate().find_map(|(i, n)| {
                let gid = pg + 1 + i as u32;
                (!first16.contains(&gid) && is_member(&user, n).0).then(|| (n.clone(), gid))
            });
            let Some((gname, gid)) = chosen else {
                c.anomalies.push(format!("F18g: no created group outside first16 {first16:?} (full {full:?})"));
                s.anomalies = c.anomalies;
                return s;
            };
            let g = c.mkdir("G", 0, gid, 0o770);
            s.acct = acct;
            group_row(&mut s, &g);
            s.facts = json!({
                "group": gname, "gid": gid, "getgrouplist_full": full, "getgrouplist_first16": first16,
                "index_in_full": full.iter().position(|x| *x == gid),
                "dsmemberutil": is_member(&user, &gname).1,
            });
        }
        "F20g" => {
            let user = "uprb20".to_string();
            let pg = 620_000u32;
            create_group("gprb20p", pg, &[], &mut c.anomalies);
            create_group("gprb20b", pg + 1, &[&user], &mut c.anomalies);
            create_group("gprb20a", pg + 2, &[], &mut c.anomalies);
            let (_, guid) = run("dscl", &[".", "-read", "/Groups/gprb20b", "GeneratedUID"]);
            let guid = guid.split_whitespace().nth(1).unwrap_or_default().to_string();
            must("dscl", &[".", "-append", "/Groups/gprb20a", "NestedGroups", &guid], &mut c.anomalies);
            create_user(&user, 620_100, pg, &mut c.anomalies);
            s.users.push(user.clone());
            s.groups.extend(["gprb20p", "gprb20b", "gprb20a"].map(String::from));
            let acct = sys::getpwnam(&user).expect("fresh user resolves");
            let (member, out) = is_member(&user, "gprb20a");
            if !member {
                c.anomalies.push(format!("F20g: dsmemberutil says not a member of A: {out}"));
            }
            let first16 = sys::getgrouplist(&user, acct.gid, 16);
            let g = c.mkdir("G", 0, pg + 2, 0o770);
            s.acct = acct;
            group_row(&mut s, &g);
            s.facts = json!({
                "group": "gprb20a", "gid": pg + 2, "getgrouplist_first16": first16,
                "a_in_first16": first16.contains(&(pg + 2)), "dsmemberutil": out,
            });
        }
        "F16l" => {
            s.acct = root();
            s.targets = vec![t("WX", W | X, Path::new("/usr/share"))];
            s.log = Some(PathBuf::from(format!("/usr/share/goetia-probe-{run_id}-F16l.log")));
        }
        "F17l" => {
            s.acct = root();
            let csr = run("csrutil", &["status"]).1;
            let ls = run("ls", &["-lOd", "/Library/Apple"]).1;
            s.facts = json!({ "csrutil": csr, "ls": ls });
            if !(csr.contains("enabled") && ls.contains("restricted")) {
                s.unavailable = Some("SIP not enabled or /Library/Apple not restricted".into());
            }
            s.targets = vec![t("WX", W | X, Path::new("/Library/Apple"))];
            s.log = Some(PathBuf::from(format!("/Library/Apple/goetia-probe-{run_id}-F17l.log")));
        }
        other => panic!("unknown row {other}"),
    }
    s.anomalies = c.anomalies;
    s
}

fn group_row(s: &mut Spec, g: &Path) {
    s.kind = Kind::Groups;
    s.must_run = true;
    s.run_static = true;
    s.job_needs = vec![t("X", X, g), t("WX", W | X, g)];
    s.targets = s.job_needs.clone();
}

pub fn delete_accounts(s: &Spec) -> Vec<String> {
    let mut errs = Vec::new();
    for u in &s.users {
        must("dscl", &[".", "-delete", &format!("/Users/{u}")], &mut errs);
    }
    for g in &s.groups {
        must("dscl", &[".", "-delete", &format!("/Groups/{g}")], &mut errs);
    }
    errs
}

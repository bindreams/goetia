//! Groups N and M: loopback NFS (A1 Task 1's design, ported) and user mounts. Needs nfsd, so it
//! only ever runs in the `nfs` job.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use super::common::*;
use super::launch::{self, JobSpec};
use crate::sys::{self, Pw};

pub fn export(ctx: &Ctx) -> PathBuf {
    PathBuf::from(format!("/private/var/goetia-probe-nfs-{}", ctx.run))
}

pub fn mnt1(ctx: &Ctx) -> PathBuf {
    PathBuf::from(format!("/private/var/goetia-probe-nfsmnt-{}", ctx.run))
}

pub fn mnt2(ctx: &Ctx) -> PathBuf {
    PathBuf::from(format!("/private/var/goetia-probe-nfsmnt2-{}", ctx.run))
}

fn account(ctx: &Ctx) -> Option<Pw> {
    sys::getpwnam(&format!("_gnfs-{}", ctx.run))
}

fn nfsd_running() -> bool {
    cmd("launchctl", &["print", "system/com.apple.nfsd"])
        .stdout
        .lines()
        .any(|l| l.trim() == "state = running")
}

fn showmount() -> Out {
    cmd("showmount", &["-e", "127.0.0.1"])
}

fn exports_line(ctx: &Ctx, fspath: Option<&str>) -> String {
    match fspath {
        None => format!("{} -maproot=nobody 127.0.0.1", export(ctx).display()),
        Some(f) => format!("{} -fspath={f} -maproot=nobody 127.0.0.1", export(ctx).display()),
    }
}

/// Replaces `old` by `new` in /etc/exports (or appends `new` when `old` is absent).
fn rewrite_exports(old: Option<&str>, new: &str) -> Result<(), String> {
    let cur = fs::read_to_string("/etc/exports").unwrap_or_default();
    let mut lines: Vec<&str> = cur.lines().collect();
    match old.and_then(|o| lines.iter().position(|l| *l == o)) {
        Some(i) => lines[i] = new,
        None => lines.push(new),
    }
    fs::write("/etc/exports", lines.join("\n") + "\n").map_err(|e| format!("write /etc/exports: {e}"))
}

// Setup ----------------------------------------------------------------------------------------------

/// A1 Task 1 setup steps 1 to 7, then the fixture account and its server-side directories.
pub fn setup(ctx: &mut Ctx) {
    let mut r = Res::new("N0.setup", "N", &["A1 Q9b (i)", "decision sheet 10"]);
    let mut ready = Res::new("N5", "N", &["A1 pending list: sheet deputy ruling 1"]);
    let (exp, m1, m2) = (export(ctx), mnt1(ctx), mnt2(ctx));
    // 1. nfsd's prior state.
    let enabled = cmd("nfsd", &["status"]).ok();
    let running = nfsd_running();
    ctx.state("nfsd_enabled_before", if enabled { "1" } else { "0" });
    ctx.state("nfsd_running_before", if running { "1" } else { "0" });
    // 2. The export and the mount points.
    for d in [&exp, &m1, &m2] {
        if let Err(e) = scratch_dir(ctx, d) {
            r.anomaly(e);
        }
    }
    // The fixture account first, then its server-side directories, before any lookup.
    let acct = create_account(ctx, "nfs", None, &mut r.anomalies);
    if let Some(a) = &acct {
        fixture(ctx, &exp, a, &mut r.anomalies);
    }
    // 3. /etc/exports.
    let line = exports_line(ctx, None);
    ctx.state("exports_line", &line);
    let existed = Path::new("/etc/exports").exists();
    if existed {
        let backup =
            PathBuf::from(env_or("RUNNER_TEMP", "/tmp")).join(format!("goetia-probe-exports-backup-{}", ctx.run));
        ctx.state("exports_backup", &backup.to_string_lossy());
        ctx.state("exports", "appended");
        if let Err(e) = fs::copy("/etc/exports", &backup)
            .map_err(|e| e.to_string())
            .and_then(|_| rewrite_exports(None, &line))
        {
            r.anomaly(e);
        }
    } else {
        ctx.state("exports", "created");
        if let Err(e) = rewrite_exports(None, &line) {
            r.anomaly(e);
        }
    }
    // 4. checkexports, enable, start or update.
    must(&mut r.anomalies, "nfsd", &["checkexports"]);
    ctx.state("nfsd_touched", "1");
    if !enabled {
        must(&mut r.anomalies, "nfsd", &["enable"]);
    }
    must(&mut r.anomalies, "nfsd", &[if running { "update" } else { "start" }]);
    // 5. Readiness: no wait. One look at showmount, one pre-decided fallback on `<offline>`.
    let mut forms = vec![json!({ "line": line })];
    let first = showmount();
    let mut offline = first.both().contains("<offline>");
    let mut fallback = Value::Null;
    forms[0]["showmount"] = first.to_json();
    if offline {
        let df = cmd("df", &["-P", &exp.to_string_lossy()]).stdout;
        let mp = df
            .lines()
            .nth(1)
            .and_then(|l| l.split_whitespace().last())
            .unwrap_or("/System/Volumes/Data")
            .to_string();
        let line2 = exports_line(ctx, Some(&mp));
        ctx.state("exports_line", &line2);
        if let Err(e) = rewrite_exports(Some(&line), &line2) {
            r.anomaly(e);
        }
        must(&mut r.anomalies, "nfsd", &["update"]);
        let second = showmount();
        fallback = json!({ "fspath": mp, "line": line2, "showmount": second.to_json() });
        if second.both().contains("<offline>") {
            r.anomaly(format!(
                "nfsd never exported {}: showmount says {}",
                exp.display(),
                second.both().trim()
            ));
        }
    }
    ready.observed = json!({ "polls": 0, "offline_seen_first": offline, "forms_tried": forms, "fallback": fallback });
    offline = !fallback.is_null();
    ready.differs = Some(offline);
    // 6. Mount 1, and mount 2 with noopaque_auth (undocumented: a rejection is data).
    ctx.state("mount", &m1.to_string_lossy());
    let o1 = cmd(
        "mount",
        &[
            "-t",
            "nfs",
            "-o",
            "vers=3,inet,soft",
            &format!("127.0.0.1:{}", exp.display()),
            &m1.to_string_lossy(),
        ],
    );
    if !o1.ok() {
        r.anomaly(format!("mount 1: {}", o1.both().trim()));
    }
    ctx.state("mount", &m2.to_string_lossy());
    let o2 = cmd(
        "mount",
        &[
            "-t",
            "nfs",
            "-o",
            "vers=3,inet,soft,noopaque_auth",
            &format!("127.0.0.1:{}", exp.display()),
            &m2.to_string_lossy(),
        ],
    );
    r.observed = json!({
        "nfsd_enabled_before": enabled, "nfsd_running_before": running, "export": exp, "exports_existed": existed,
        "mount1": o1.to_json(), "mount2_noopaque_auth": o2.to_json(), "mount2_accepted": o2.ok(),
        "mount_table": sh("mount | grep goetia-probe"),
    });
    ctx.emit(r);
    ctx.emit(ready);
}

/// Server-side, under the export: allowed/ (0755), private/ (0700), acl-denied/ (0755 + deny ACE).
fn fixture(ctx: &Ctx, exp: &Path, a: &Pw, anomalies: &mut Vec<String>) {
    for (name, mode) in [("allowed", 0o755), ("private", 0o700), ("acl-denied", 0o755)] {
        let d = exp.join(name);
        if let Err(e) = mkdir_mode(&d, a.uid, a.gid, mode) {
            anomalies.push(e);
        }
        if name != "acl-denied" {
            if let Err(e) = write_file(&d.join("inner"), a.uid, a.gid, 0o644, b"x") {
                anomalies.push(e);
            }
        }
    }
    let ace = format!("user:{} deny write,add_file,search", a.name);
    must(
        anomalies,
        "chmod",
        &["+a", &ace, &exp.join("acl-denied").to_string_lossy()],
    );
    let _ = ctx;
}

// Helpers as the account --------------------------------------------------------------------------

fn helper(ctx: &Ctx, user: &str, args: &[&str]) -> Value {
    let bin = ctx.base().join("bin/launchd-probe");
    let mut a = vec!["step0", "helper"];
    a.extend_from_slice(args);
    let o = as_user(user, &bin.to_string_lossy(), &a);
    let kv: serde_json::Map<String, Value> = o
        .stdout
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), json!(v)))
        .collect();
    json!({ "code": o.code, "values": kv, "stderr": o.stderr })
}

fn errno_of(v: &Value, key: &str) -> Option<i64> {
    v["values"][key].as_str()?.parse().ok()
}

// N1 to N4 -----------------------------------------------------------------------------------------

fn need_account(ctx: &mut Ctx, r: &mut Res) -> Option<(Pw, PathBuf, PathBuf)> {
    let a = account(ctx);
    if a.is_none() {
        r.anomaly("account _gnfs missing: did nfs-setup run?");
    }
    if !mnt1(ctx).join("allowed").exists() {
        r.anomaly("mount 1 not usable (allowed/ absent): did nfs-setup mount?");
        return None;
    }
    a.map(|a| (a, mnt1(ctx), mnt2(ctx)))
}

fn open_dir_pathconf(p: &Path) -> Value {
    let c = sys::cpath(p);
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        return json!({ "open_errno": sys::errno() });
    }
    let v = unsafe { libc::fpathconf(fd, 14) }; // _PC_AUTH_OPAQUE_NP
    let e = if v < 0 { sys::errno() } else { 0 };
    unsafe { libc::close(fd) };
    json!({ "value": v, "errno": e })
}

pub fn n1(ctx: &mut Ctx) {
    let mut r = Res::new("N1", "N", &["decision sheet 10", "A1 Q9b (i)"]).expect(json!(
        "!MNT_LOCAL on the mount; pathconf 1 on vers=3, 0 on noopaque_auth"
    ));
    let Some((_, m1, m2)) = need_account(ctx, &mut r) else {
        return ctx.emit(r);
    };
    let s = statfs_json(&m1);
    let local = s["flags_named"].as_array().is_some_and(|a| a.contains(&json!("LOCAL")));
    let p1 = open_dir_pathconf(&m1);
    let mount2_up = m2.join("allowed").exists();
    let p2 = if mount2_up { open_dir_pathconf(&m2) } else { Value::Null };
    if !mount2_up {
        r.verdict = "unavailable".into();
    }
    r.differs = Some(local || p1["value"] != json!(1) || (mount2_up && p2["value"] != json!(0)));
    r.observed = json!({ "statfs_mount1": s, "mnt_local": local, "pathconf_vers3": p1, "pathconf_noopaque_auth": p2, "mount2_available": mount2_up, "nfsstat_m": cmd("nfsstat", &["-m"]).both() });
    ctx.emit(r);
}

pub fn n2(ctx: &mut Ctx) {
    let mut r = Res::new("N2", "N", &["A1 Q9b (i) (root squash)"])
        .expect(json!("root: EACCES into private/; the account's open works"));
    let Some((a, m1, _)) = need_account(ctx, &mut r) else {
        return ctx.emit(r);
    };
    let root_stat = |d: &str| {
        let c = sys::cpath(&m1.join(d).join("inner"));
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatat(libc::AT_FDCWD, c.as_ptr(), &mut st, 0) } == 0 {
            0
        } else {
            sys::errno()
        }
    };
    let (rp, ra) = (root_stat("private"), root_stat("allowed"));
    if ra != 0 {
        r.anomaly(format!("control: root fstatat allowed/inner errno {ra}"));
    }
    let acct = helper(
        ctx,
        &a.name,
        &["search-stat", &m1.join("private").to_string_lossy(), "inner"],
    );
    r.differs = Some(rp != libc::EACCES || errno_of(&acct, "open") != Some(0) || errno_of(&acct, "fstatat") != Some(0));
    r.observed = json!({ "root_fstatat_private_inner": rp, "root_fstatat_allowed_inner_control": ra, "account_search_open_then_fstatat": acct });
    ctx.emit(r);
}

pub fn n3(ctx: &mut Ctx) {
    let mut r = Res::new("N3", "N", &["A1 Q9b (i)"]).expect(json!("denied on acl-denied/, allowed on allowed/"));
    let Some((a, m1, _)) = need_account(ctx, &mut r) else {
        return ctx.emit(r);
    };
    let w = sys::W.to_string();
    let denied = helper(ctx, &a.name, &["access", &w, &m1.join("acl-denied").to_string_lossy()]);
    let allowed = helper(ctx, &a.name, &["access", &w, &m1.join("allowed").to_string_lossy()]);
    if errno_of(&allowed, "errno") != Some(0) {
        r.anomaly(format!("control: _WRITE_OK on allowed/ is {allowed}"));
    }
    r.differs = Some(errno_of(&denied, "errno") == Some(0) || errno_of(&allowed, "errno") != Some(0));
    r.observed = json!({ "acl_denied": denied, "allowed": allowed, "server_side_acl": cmd("ls", &["-leOd", &export(ctx).join("acl-denied").to_string_lossy()]).both() });
    ctx.emit(r);
}

pub fn n4(ctx: &mut Ctx) {
    let mut probe = Res::new("N4.pre", "N", &["decision sheet 10"]);
    let Some((a, m1, _)) = need_account(ctx, &mut probe) else {
        probe.id = "N4".into();
        return ctx.emit(probe);
    };
    for (dir, expect) in [("allowed", "ran"), ("acl-denied", "refused")] {
        for usage in ["cwd", "log"] {
            let mut r =
                Res::new(&format!("N4.{dir}.{usage}"), "N", &["decision sheet 10", "A1 Q9b (i)"]).expect(json!(expect));
            let mut spec = JobSpec::new(
                &format!("n4-{dir}-{usage}"),
                &a.name,
                Path::new("/Library/LaunchDaemons"),
            );
            let d = m1.join(dir);
            if usage == "cwd" {
                spec.cwd = Some(d.clone());
            } else {
                spec.log = Some(d.join("out.log"));
            }
            let o = launch::launch(ctx, &spec);
            o.apply(&mut r);
            if dir == "allowed" && o.verdict != "ran" {
                r.anomaly(format!("control: allowed/ {usage} is {}", o.verdict));
            }
            r.differs = Some(expect != o.verdict);
            r.observed = json!({ "dir": d, "use": usage, "detail": o.detail() });
            ctx.emit(r);
        }
    }
    if !probe.anomalies.is_empty() {
        probe.id = "N4".into();
        ctx.emit(probe);
    }
}

// Group M ----------------------------------------------------------------------------------------------

fn getfsstat_entries() -> Vec<Value> {
    let n = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if n <= 0 {
        return vec![];
    }
    let mut buf: Vec<libc::statfs> = vec![unsafe { std::mem::zeroed() }; n as usize + 8];
    let got = unsafe {
        libc::getfsstat(
            buf.as_mut_ptr(),
            (buf.len() * std::mem::size_of::<libc::statfs>()) as libc::c_int,
            libc::MNT_NOWAIT,
        )
    };
    buf.truncate(got.max(0) as usize);
    buf.iter().map(statfs_value).collect()
}

fn user_mount_dirs(ctx: &Ctx) -> PathBuf {
    PathBuf::from(format!("/Library/Application Support/goetia-probe-{}", ctx.run))
}

pub fn m12(ctx: &mut Ctx) {
    let mut m1 = Res::new("M1", "M", &["B0 Q-S7"]).expect(json!(
        "a non-root user mounts over a directory it owns; EPERM over one it does not"
    ));
    let mut m2 = Res::new("M2", "M", &["B0 Q-S7 (the pre-check's string match)"])
        .expect(json!("path as spelled, with or without /System/Volumes/Data"));
    let exp = export(ctx);
    let parent = user_mount_dirs(ctx);
    ctx.state("dir", &parent.to_string_lossy());
    if let Err(e) = mkdir_mode(&parent, 0, 0, 0o777) {
        m1.anomaly(e);
    }
    let _ = fs::set_permissions(&parent, std::os::unix::fs::PermissionsExt::from_mode(0o777));
    let Some(u) = create_account(ctx, "mnt", None, &mut m1.anomalies) else {
        return ctx.emit(m1);
    };
    let (owned, notowned) = (parent.join("owned"), parent.join("notowned"));
    for (d, uid) in [(&owned, u.uid), (&notowned, 0)] {
        if let Err(e) = mkdir_mode(d, uid, if uid == 0 { 0 } else { u.gid }, 0o755) {
            m1.anomaly(e);
        }
    }
    let nfs_source = format!("127.0.0.1:{}", exp.display());
    // Under a root-owned 0755 directory of its own, so the account can traverse to it.
    let imgdir = PathBuf::from(format!("/private/var/goetia-probe-m1img-{}", ctx.run));
    if let Err(e) = scratch_dir(ctx, &imgdir) {
        m1.anomaly(e);
    }
    let img = imgdir.join("m1.dmg");
    ctx.state("image", &img.to_string_lossy());
    must(
        &mut m1.anomalies,
        "hdiutil",
        &[
            "create",
            "-size",
            "8m",
            "-fs",
            "HFS+",
            "-volname",
            "GPM1",
            &img.to_string_lossy(),
        ],
    );
    let _ = fs::set_permissions(&img, std::os::unix::fs::PermissionsExt::from_mode(0o644));
    let readable = as_user(&u.name, "/usr/bin/head", &["-c", "1", &img.to_string_lossy()]);
    let mut attempts = vec![];
    let mut mounted: Vec<(&str, PathBuf)> = vec![];
    let tries: [(&str, &PathBuf); 3] = [
        ("a-hdiutil-owned", &owned),
        ("b-mount_nfs-owned", &owned),
        ("c-mount_nfs-notowned", &notowned),
    ];
    for (name, target) in tries {
        ctx.state("mount", &target.to_string_lossy());
        let t = target.to_string_lossy().into_owned();
        let o = match name {
            "a-hdiutil-owned" => as_user(
                &u.name,
                "hdiutil",
                &["attach", "-nobrowse", "-mountpoint", &t, &img.to_string_lossy()],
            ),
            _ => as_user(&u.name, "/sbin/mount_nfs", &["-o", "vers=3,inet,soft", &nfs_source, &t]),
        };
        let line = sh(&format!("mount | grep -F -- '{t}'")).trim().to_string();
        attempts.push(
            json!({ "attempt": name, "target": t, "result": o.to_json(), "succeeded": o.ok(), "mount_line": line }),
        );
        if o.ok() {
            mounted.push((name, target.clone()));
            // The (a) mount must come off before (b) can use the same directory.
            if name == "a-hdiutil-owned" {
                let m = cmd("hdiutil", &["detach", &t]);
                attempts.last_mut().unwrap()["detach"] = m.to_json();
                if !m.ok() {
                    m1.anomaly(format!("detach after (a): {}", m.both().trim()));
                }
                continue;
            }
        }
    }
    let ok = |n: &str| {
        attempts
            .iter()
            .any(|a| a["attempt"] == n && a["succeeded"] == json!(true))
    };
    m1.differs = Some(!ok("a-hdiutil-owned") || !ok("b-mount_nfs-owned") || ok("c-mount_nfs-notowned"));
    m1.observed = json!({ "account": pw_json(&u), "image_readable_by_account": readable.ok(), "image_read": readable.to_json(), "attempts": attempts });
    // M2: the getfsstat entry of each mount that is still up (the NFS ones; (a) was detached to free the directory).
    let entries = getfsstat_entries();
    let parent_s = parent.to_string_lossy().into_owned();
    let ours: Vec<&Value> = entries
        .iter()
        .filter(|e| e["mntonname"].as_str().is_some_and(|m| m.contains("goetia-probe")))
        .collect();
    m2.observed = json!({ "mounts_listed": ours, "spelled_target": parent_s, "mounted_by_this_step": mounted.iter().map(|(n, p)| json!({"attempt": n, "path": p})).collect::<Vec<_>>() });
    m2.differs = Some(ours.iter().any(|e| {
        e["mntonname"]
            .as_str()
            .is_some_and(|m| m.starts_with("/System/Volumes/Data"))
    }));
    ctx.emit(m1);
    ctx.emit(m2);
}

/// M3: `getfsstat(MNT_NOWAIT)` while a `stat` of a stalled hard mount is blocked.
pub fn m3(ctx: &mut Ctx) {
    let mut r = Res::new("M3", "M", &["B0 Q-S7 (the pre-check)"])
        .expect(json!("getfsstat(MNT_NOWAIT) returns while the stat is blocked"));
    let Some(u) = sys::getpwnam(&format!("_gmnt-{}", ctx.run)) else {
        r.anomaly("account _gmnt missing: did the M1 step run?");
        return ctx.emit(r);
    };
    let exp = export(ctx);
    let stall = user_mount_dirs(ctx).join("stall");
    if let Err(e) = mkdir_mode(&stall, u.uid, u.gid, 0o755) {
        r.anomaly(e);
        return ctx.emit(r);
    }
    ctx.state("mount-force", &stall.to_string_lossy());
    let t = stall.to_string_lossy().into_owned();
    let mo = as_user(
        &u.name,
        "/sbin/mount_nfs",
        &["-o", "vers=3,inet,hard", &format!("127.0.0.1:{}", exp.display()), &t],
    );
    if !mo.ok() {
        r.anomaly(format!("hard mount: {}", mo.both().trim()));
        return ctx.emit(r);
    }
    let stop = cmd("nfsd", &["stop"]);
    // The child writes `pre`, then lstats the mount root and an uncached name (a LOOKUP RPC).
    let bin = ctx.base().join("bin/launchd-probe");
    let mut child = Command::new("/usr/bin/sudo")
        .args([
            "-n",
            "-u",
            &u.name,
            &bin.to_string_lossy(),
            "step0",
            "helper",
            "stall",
            &t,
        ])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn stall child");
    let mut lines = BufReader::new(child.stdout.take().unwrap());
    let mut pre = String::new();
    lines.read_line(&mut pre).ok();
    let pid = child.id();
    ctx.state("pid", &pid.to_string());
    let ps = |p: u32| {
        cmd("ps", &["-o", "state=", "-p", &p.to_string()])
            .stdout
            .trim()
            .to_string()
    };
    let child_before = children_states(pid);
    let t0 = std::time::Instant::now();
    let entries = getfsstat_entries();
    let returned = t0.elapsed().as_secs_f64();
    let listed = entries
        .iter()
        .any(|e| e["mntonname"].as_str().is_some_and(|m| m.ends_with("/stall")));
    let child_after = children_states(pid);
    let sudo_state = ps(pid);
    let sample = cmd("sample", &[&pid.to_string(), "1"])
        .stdout
        .lines()
        .take(40)
        .map(str::to_string)
        .collect::<Vec<_>>();
    let stalled = child_after.iter().any(|(_, s)| s.contains('U'));
    // The parent never waits for the child: kill it, force the unmount.
    let _ = cmd("pkill", &["-KILL", "-P", &pid.to_string()]);
    let _ = child.kill();
    let um = cmd("umount", &["-f", &t]);
    if !um.ok() {
        r.anomaly(format!("umount -f {t}: {}", um.both().trim()));
    }
    let _ = child.wait();
    r.verdict = if stalled { "value".into() } else { "inconclusive".into() };
    r.differs = Some(!listed);
    r.observed = json!({
        "nfsd_stop": stop.to_json(), "child_pre_line": pre.trim(), "getfsstat_returned": true, "getfsstat_seconds": returned,
        "mount_listed": listed, "child_states_before_getfsstat": child_before, "child_states_after_getfsstat": child_after,
        "sudo_state": sudo_state, "stall_reproduced": stalled, "sample_top": sample,
        "note": "inconclusive means the stall was not reproduced, not that getfsstat failed",
    });
    ctx.emit(r);
}

/// `(pid, state)` for the sudo process and its descendants one level down.
fn children_states(pid: u32) -> Vec<(u32, String)> {
    let all = cmd("ps", &["-A", "-o", "pid=,ppid=,state="]).stdout;
    let rows: Vec<(u32, u32, String)> = all
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((
                it.next()?.parse().ok()?,
                it.next()?.parse().ok()?,
                it.next()?.to_string(),
            ))
        })
        .collect();
    let mut keep = vec![pid];
    let mut out = vec![];
    let mut i = 0;
    while i < keep.len() {
        let cur = keep[i];
        for (p, pp, s) in &rows {
            if *pp == cur || *p == cur {
                if !keep.contains(p) {
                    keep.push(*p);
                }
                if !out.iter().any(|(q, _)| q == p) {
                    out.push((*p, s.clone()));
                }
            }
        }
        i += 1;
    }
    out
}

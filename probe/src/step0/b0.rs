//! Group B: B0's step 0 (B1 to B5). B6 lives in `size.rs`.

use std::fs;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::common::*;
use super::launch::{self, JobSpec};
use crate::sys;

const NOBODY: &str = "nobody";

pub fn vardir(ctx: &Ctx) -> PathBuf {
    PathBuf::from(format!("/private/var/db/goetia-probe-{}", ctx.run))
}

fn nobody_ids() -> (u32, u32) {
    let p = sys::getpwnam(NOBODY).expect("nobody exists");
    (p.uid, p.gid)
}

// B1 -----------------------------------------------------------------------------------------------

fn nlink(p: &Path) -> Option<u64> {
    fs::metadata(p).ok().map(|m| m.nlink())
}

fn mkfifo(p: &Path) -> Result<(), String> {
    let c = sys::cpath(p);
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return Err(format!("mkfifo {}: errno {}", p.display(), sys::errno()));
    }
    Ok(())
}

/// One entry of the kind `op` in `dir`. `f` must exist for `hardlink` and `clone`.
fn make_entry(op: &str, dir: &Path) -> Result<(), String> {
    let e = |r: std::io::Result<()>| r.map_err(|e| format!("{op}: {e}"));
    match op {
        "file" => e(fs::write(dir.join("f"), b"x")),
        "directory" => e(fs::create_dir(dir.join("d"))),
        "dotfile" => e(fs::write(dir.join(".dot"), b"x")),
        "symlink" => e(symlink("f", dir.join("s"))),
        "hardlink" => e(fs::hard_link(dir.join("f"), dir.join("h"))),
        "fifo" => mkfifo(&dir.join("p")),
        "clone" => {
            let o = cmd(
                "cp",
                &["-c", &dir.join("f").to_string_lossy(), &dir.join("c").to_string_lossy()],
            );
            if o.ok() {
                Ok(())
            } else {
                Err(format!("cp -c: {}", o.both().trim()))
            }
        }
        _ => Err(format!("unknown op {op}")),
    }
}

const B1_OPS: [&str; 7] = ["file", "directory", "dotfile", "symlink", "hardlink", "fifo", "clone"];

pub fn b1(ctx: &mut Ctx) {
    let mut r =
        Res::new("B1", "B", &["B0 Q-S7 (ii)"]).expect(json!("2 for an empty directory, then +1 per entry of any kind"));
    let root = PathBuf::from(format!("/private/var/goetia-probe-b1-{}", ctx.run));
    if let Err(e) = scratch_dir(ctx, &root) {
        r.anomaly(e);
        return ctx.emit(r);
    }
    // Cumulative: one directory, every kind added in turn.
    let cum = root.join("cumulative");
    let mut seq = vec![];
    if let Err(e) = fs::create_dir(&cum) {
        r.anomaly(format!("mkdir cumulative: {e}"));
        return ctx.emit(r);
    }
    seq.push(json!({ "after": "empty", "nlink": nlink(&cum) }));
    let mut counts = vec![nlink(&cum)];
    for op in B1_OPS {
        if let Err(e) = make_entry(op, &cum) {
            r.anomaly(e);
        }
        counts.push(nlink(&cum));
        seq.push(json!({ "after": op, "nlink": nlink(&cum) }));
    }
    // Isolated: a fresh directory per kind (with `f` first where the kind needs it).
    let mut iso = vec![];
    let mut deltas_ok = true;
    for op in B1_OPS {
        let d = root.join(format!("iso-{op}"));
        if let Err(e) = fs::create_dir(&d) {
            r.anomaly(format!("mkdir {}: {e}", d.display()));
            continue;
        }
        if matches!(op, "hardlink" | "clone" | "symlink") {
            let _ = make_entry("file", &d);
        }
        let before = nlink(&d);
        if let Err(e) = make_entry(op, &d) {
            r.anomaly(e);
        }
        let after = nlink(&d);
        deltas_ok &= matches!((before, after), (Some(b), Some(a)) if a == b + 1);
        iso.push(json!({ "op": op, "before": before, "after": after }));
    }
    if counts[0] != Some(2) {
        r.anomaly(format!("control: empty directory st_nlink {:?}, want 2", counts[0]));
    }
    let cum_ok = counts
        .windows(2)
        .all(|w| matches!((w[0], w[1]), (Some(a), Some(b)) if b == a + 1));
    r.differs = Some(!(cum_ok && deltas_ok && counts[0] == Some(2)));
    r.observed = json!({
        "cumulative": seq, "isolated": iso,
        "fstype_library_app_support": statfs_json(Path::new("/Library/Application Support"))["fstypename"],
        "fstype_probe_dir": statfs_json(&root)["fstypename"],
    });
    ctx.emit(r);
}

// B2 -----------------------------------------------------------------------------------------------

pub fn ensure_vardir(ctx: &Ctx) -> Result<PathBuf, String> {
    let v = vardir(ctx);
    if !v.exists() {
        scratch_dir(ctx, &v)?;
    }
    Ok(v)
}

fn fsid(v: &Value) -> Value {
    v["fsid"].clone()
}

pub fn b2(ctx: &mut Ctx) {
    let mut r = Res::new("B2", "B", &["B0 Q-S5 (location)"])
        .expect(json!("f_fsid of the vardir equals /Library/LaunchDaemons's"));
    let v = match ensure_vardir(ctx) {
        Ok(v) => v,
        Err(e) => {
            r.anomaly(e);
            return ctx.emit(r);
        }
    };
    let paths = [
        ("vardir", v.clone()),
        ("library_launchdaemons", PathBuf::from("/Library/LaunchDaemons")),
        ("library_app_support", PathBuf::from("/Library/Application Support")),
        ("private_var_db", PathBuf::from("/private/var/db")),
        ("root", PathBuf::from("/")),
    ];
    let mut obs = json!({});
    for (k, p) in &paths {
        let mut s = statfs_json(p);
        s["st_dev"] = json!(st_dev(p));
        obs[*k] = s;
    }
    obs["csrutil"] = json!(sh("csrutil status").trim());
    obs["ls_private_var_db"] = json!(cmd("ls", &["-lO@e", "/private/var/db"]).both());
    let ld = fsid(&obs["library_launchdaemons"]);
    if fsid(&obs["private_var_db"]) != ld {
        r.anomaly(format!(
            "control: f_fsid of /private/var/db {} != /Library/LaunchDaemons {ld}",
            fsid(&obs["private_var_db"])
        ));
    }
    r.differs = Some(fsid(&obs["vardir"]) != ld);
    r.observed = obs;
    ctx.emit(r);
}

// BTM ----------------------------------------------------------------------------------------------

pub fn btm_snapshot(ctx: &Ctx, tag: &str, needles: &[String]) -> Value {
    let o = cmd("sfltool", &["dumpbtm"]);
    let file = ctx.results.join(format!("{tag}.btm.txt"));
    let _ = fs::write(&file, o.both());
    if !o.ok() {
        return json!({ "available": false, "error": o.both().trim(), "code": o.code });
    }
    let text = o.stdout;
    let uuids = text.lines().filter(|l| l.trim_start().starts_with("UUID:")).count();
    let mentions: serde_json::Map<String, Value> = needles
        .iter()
        .map(|n| (n.clone(), json!(text.matches(n.as_str()).count())))
        .collect();
    json!({ "available": true, "uuid_lines": uuids, "lines": text.lines().count(), "bytes": text.len(), "mentions": mentions })
}

fn btm_changed(a: &Value, b: &Value) -> Option<bool> {
    if a["available"] != json!(true) || b["available"] != json!(true) {
        return None;
    }
    Some(a["uuid_lines"] != b["uuid_lines"] || a["lines"] != b["lines"] || a["mentions"] != b["mentions"])
}

// B3 -----------------------------------------------------------------------------------------------

pub fn b3(ctx: &mut Ctx) {
    let mut r = Res::new("B3", "B", &["B0 Q-S5 (location)"])
        .expect(json!("path recorded as the staged path, Ran, no new BTM record"));
    let v = match ensure_vardir(ctx) {
        Ok(v) => v,
        Err(e) => {
            r.anomaly(e);
            return ctx.emit(r);
        }
    };
    let dir = v.join("daemons");
    if let Err(e) = scratch_dir(ctx, &dir) {
        r.anomaly(e);
        return ctx.emit(r);
    }
    let label = ctx.label("b3");
    let needles = vec![label.clone(), dir.to_string_lossy().into_owned()];
    // Control first: the same sentinel from /Library/LaunchDaemons is Ran.
    let ctl = launch::launch(ctx, &JobSpec::new("b3ctl", NOBODY, Path::new("/Library/LaunchDaemons")));
    if ctl.verdict != "ran" {
        r.anomaly(format!(
            "control: sentinel from /Library/LaunchDaemons is {}",
            ctl.verdict
        ));
    }
    r.anomalies
        .extend(ctl.anomalies.iter().map(|a| format!("control: {a}")));
    let before = btm_snapshot(ctx, "B3.before", &needles);
    let probe = |ctx: &Ctx| btm_snapshot(ctx, "B3.during", &needles);
    let during_val = std::cell::RefCell::new(Value::Null);
    let cb = || {
        let v = probe(ctx);
        *during_val.borrow_mut() = v.clone();
        v
    };
    let mut spec = JobSpec::new("b3", NOBODY, &dir);
    spec.on_ready = Some(&cb);
    let o = launch::launch(ctx, &spec);
    let after = btm_snapshot(ctx, "B3.after", &needles);
    o.apply(&mut r);
    let print_path = o
        .first
        .as_ref()
        .and_then(|f| f.print_loaded.as_ref())
        .and_then(|p| p["print"].as_str())
        .and_then(|t| lifecycle_field(t, "path"));
    let during_v = during_val.borrow().clone();
    let changed = btm_changed(&before, &after).or_else(|| btm_changed(&before, &during_v));
    r.differs = Some(
        o.verdict != "ran"
            || changed == Some(true)
            || print_path.as_deref().map(|p| !p.contains(&*dir.to_string_lossy())) == Some(true),
    );
    r.observed = json!({
        "print_path_line": print_path, "staged_dir": dir, "btm_before": before, "btm_during": during_v,
        "btm_after": after, "btm_changed": changed, "control": ctl.verdict, "detail": o.detail(),
    });
    ctx.emit(r);
}

fn lifecycle_field(text: &str, key: &str) -> Option<String> {
    crate::lifecycle::field(text, key).map(|s| s.trim().to_string())
}

// B4 -----------------------------------------------------------------------------------------------

struct Case {
    id: &'static str,
    shape: &'static str,
    /// The path handed to `launchctl bootstrap`.
    path: PathBuf,
    label: String,
    expect: Option<&'static str>,
}

fn acl_everyone_write(p: &Path) -> Out {
    cmd("chmod", &["+a", "everyone allow write", &p.to_string_lossy()])
}

fn b4_fixture(ctx: &Ctx, root: &Path, anomalies: &mut Vec<String>) -> Vec<Case> {
    let (nu, ng) = nobody_ids();
    let mut cases = vec![];
    let mk = |anomalies: &mut Vec<String>,
              n: u32,
              shape: &'static str,
              expect: Option<&'static str>|
     -> (PathBuf, String, PathBuf) {
        let dir = root.join(format!("p{n}"));
        let label = ctx.label(&format!("b4p{n}"));
        if let Err(e) = mkdir_mode(&dir, 0, 0, 0o755) {
            anomalies.push(e);
        }
        let file = dir.join(format!("{label}.plist"));
        if let Err(e) = write_file(&file, 0, 0, 0o644, launch::minimal_plist(&label).as_bytes()) {
            anomalies.push(e);
        }
        let _ = (shape, expect);
        (dir, label, file)
    };
    for (n, shape, expect) in [
        (0, "root:wheel 0644 (control)", Some("accepted")),
        (1, "root:wheel 0664 (g+w)", Some("accepted")),
        (2, "root:wheel 0644 + everyone allow write ACE", Some("accepted")),
        (3, "owned by nobody, 0644", Some("refused")),
        (4, "root:wheel 0646 (o+w)", Some("refused")),
        (5, "root:wheel 0644, nlink 2", Some("accepted")),
    ] {
        let (dir, label, file) = mk(anomalies, n, shape, expect);
        match n {
            1 => {
                let _ = fs::set_permissions(&file, fs::Permissions::from_mode(0o664));
            }
            2 => {
                let o = acl_everyone_write(&file);
                if !o.ok() {
                    anomalies.push(format!("chmod +a: {}", o.both().trim()));
                }
            }
            3 => {
                let _ = std::os::unix::fs::chown(&file, Some(nu), Some(0));
            }
            4 => {
                let _ = fs::set_permissions(&file, fs::Permissions::from_mode(0o646));
            }
            5 => {
                let x = root.join("x5");
                let _ = mkdir_mode(&x, 0, 0, 0o755);
                if let Err(e) = fs::hard_link(&file, x.join("other-name")) {
                    anomalies.push(format!("hard link: {e}"));
                }
            }
            _ => {}
        }
        let _ = dir;
        cases.push(Case {
            id: ["p0", "p1", "p2", "p3", "p4", "p5"][n as usize],
            shape,
            path: file,
            label,
            expect,
        });
    }
    // p6: a root-owned symlink to a root-owned 0644 target in another root-owned directory.
    let (t6, l6, f6) = mk(anomalies, 6, "", None);
    let linkdir6 = root.join("l6");
    let _ = mkdir_mode(&linkdir6, 0, 0, 0o755);
    let link6 = linkdir6.join("link.plist");
    let _ = symlink(&f6, &link6);
    let _ = t6;
    cases.push(Case {
        id: "p6",
        shape: "root-owned symlink -> root-owned 0644 target in another root-owned dir",
        path: link6,
        label: l6,
        expect: None,
    });
    // p7: the same, the symlink owned by nobody.
    let (_, l7, f7) = mk(anomalies, 7, "", None);
    let linkdir7 = root.join("l7");
    let _ = mkdir_mode(&linkdir7, 0, 0, 0o755);
    let link7 = linkdir7.join("link.plist");
    let _ = symlink(&f7, &link7);
    if let Err(e) = std::os::unix::fs::lchown(&link7, Some(nu), Some(ng)) {
        anomalies.push(format!("lchown: {e}"));
    }
    cases.push(Case {
        id: "p7",
        shape: "nobody-owned symlink -> root-owned 0644 target",
        path: link7,
        label: l7,
        expect: None,
    });
    // p8: a root-owned symlink to a root-owned 0644 target, both inside a nobody-writable directory.
    let d8 = root.join("p8");
    let l8 = ctx.label("b4p8");
    let _ = mkdir_mode(&d8, nu, ng, 0o755);
    let f8 = d8.join(format!("{l8}.plist"));
    if let Err(e) = write_file(&f8, 0, 0, 0o644, launch::minimal_plist(&l8).as_bytes()) {
        anomalies.push(e);
    }
    let link8 = d8.join("link.plist");
    let _ = symlink(&f8, &link8);
    cases.push(Case {
        id: "p8",
        shape: "root-owned symlink -> root-owned 0644 target, both in a nobody-owned dir",
        path: link8,
        label: l8,
        expect: None,
    });
    cases
}

pub fn b4(ctx: &mut Ctx) {
    let root = PathBuf::from(format!("/private/var/goetia-probe-b4-{}", ctx.run));
    let mut fx_anoms = vec![];
    if let Err(e) = scratch_dir(ctx, &root) {
        fx_anoms.push(e);
    }
    let cases = b4_fixture(ctx, &root, &mut fx_anoms);
    // Fixture self-check for p0 to p5: the file is what the table says.
    let (nu, _) = nobody_ids();
    for (c, (mode, uid, links)) in cases.iter().take(6).zip([
        (0o644, 0, 1),
        (0o664, 0, 1),
        (0o644, 0, 1),
        (0o644, nu, 1),
        (0o646, 0, 1),
        (0o644, 0, 2),
    ]) {
        match fs::metadata(&c.path) {
            Ok(m) if m.mode() & 0o7777 == mode && m.uid() == uid && m.nlink() == links => {}
            other => fx_anoms.push(format!(
                "fixture {}: {:?} (want mode {mode:o} uid {uid} nlink {links})",
                c.id,
                other.map(|m| (m.mode() & 0o7777, m.uid(), m.nlink()))
            )),
        }
    }
    let mut results = vec![];
    for c in &cases {
        let mut r = Res::new(
            &format!("B4.{}", c.id),
            "B",
            &["B0 Q-S3 (stricter vet)", "decision sheet 17"],
        );
        r.expected = json!(c.expect);
        ctx.state("label", &c.label);
        let ls = cmd("ls", &["-leOd", &c.path.to_string_lossy()]).both();
        let boot = cmd("launchctl", &["bootstrap", "system", &c.path.to_string_lossy()]);
        let accepted = boot.ok();
        let mut obs =
            json!({ "shape": c.shape, "path": c.path, "ls": ls, "bootstrap": boot.to_json(), "accepted": accepted });
        if accepted {
            let p = cmd("launchctl", &["print", &format!("system/{}", c.label)]);
            obs["print"] = launch::print_summary(&p.stdout);
            let bo = cmd("launchctl", &["bootout", &format!("system/{}", c.label)]);
            obs["bootout"] = bo.to_json();
            if !bo.ok() {
                r.anomaly(format!("bootout {}: {}", c.label, bo.both().trim()));
            }
        }
        r.verdict = if accepted {
            "value".into()
        } else {
            "bootstrap-refused".into()
        };
        r.differs = c.expect.map(|e| e != if accepted { "accepted" } else { "refused" });
        r.observed = obs;
        if c.id == "p0" && !accepted {
            r.anomaly("control p0 (root:wheel 0644) refused");
        }
        if c.id == "p4" && accepted {
            r.anomaly("control p4 (o+w, the known-refused case) accepted");
        }
        results.push(r);
    }
    for a in fx_anoms {
        if let Some(first) = results.first_mut() {
            first.anomaly(a);
        }
    }
    for r in results {
        ctx.emit(r);
    }
}

// B5 -----------------------------------------------------------------------------------------------

pub fn b5(ctx: &mut Ctx) {
    let mut r = Res::new("B5", "B", &["B0 Q-S3 (e)"])
        .expect(json!("job still loaded, no BTM change after a fresh-inode rewrite"));
    // `ctl` is bootstrapped alongside and never rewritten: a record that appears for it as well
    // is BTM's own lag, not an effect of the rewrite.
    let (label, ctl_label) = (ctx.label("b5"), ctx.label("b5ctl"));
    let path = PathBuf::from(format!("/Library/LaunchDaemons/{label}.plist"));
    let ctl_path = PathBuf::from(format!("/Library/LaunchDaemons/{ctl_label}.plist"));
    let needles = vec![
        label.clone(),
        path.to_string_lossy().into_owned(),
        ctl_label.clone(),
        ctl_path.to_string_lossy().into_owned(),
    ];
    for (l, p) in [(&label, &path), (&ctl_label, &ctl_path)] {
        ctx.state("label", l);
        ctx.state("plist", &p.to_string_lossy());
        if let Err(e) = launch::write_plist(p, launch::minimal_plist(l).as_bytes()) {
            r.anomaly(e);
            return ctx.emit(r);
        }
    }
    let disabled = cmd("launchctl", &["print-disabled", "system"]);
    let snap_pre = btm_snapshot(ctx, "B5.0-before-bootstrap", &needles);
    let boot = cmd("launchctl", &["bootstrap", "system", &path.to_string_lossy()]);
    let boot_ctl = cmd("launchctl", &["bootstrap", "system", &ctl_path.to_string_lossy()]);
    if !boot.ok() || !boot_ctl.ok() {
        r.anomaly(format!(
            "control: bootstrap of p0 shapes: {} / {}",
            boot.both().trim(),
            boot_ctl.both().trim()
        ));
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&ctl_path);
        return ctx.emit(r);
    }
    let snap_boot = btm_snapshot(ctx, "B5.1-after-bootstrap", &needles);
    let ino_before = fs::metadata(&path).map(|m| m.ino()).ok();
    let sibling = path.with_extension("plist.new");
    let rewrite = (|| -> Result<(), String> {
        let f = fs::File::create(&sibling).map_err(|e| format!("create sibling: {e}"))?;
        use std::io::Write;
        (&f).write_all(launch::minimal_plist(&label).as_bytes())
            .map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| format!("fullfsync: {e}"))?; // F_FULLFSYNC on macOS
        fs::rename(&sibling, &path).map_err(|e| format!("rename: {e}"))
    })();
    if let Err(e) = &rewrite {
        r.anomaly(format!("rewrite: {e}"));
        let _ = fs::remove_file(&sibling);
    }
    let ino_after = fs::metadata(&path).map(|m| m.ino()).ok();
    let snap_rewrite = btm_snapshot(ctx, "B5.2-after-rewrite", &needles);
    let print = cmd("launchctl", &["print", &format!("system/{label}")]);
    let still_loaded = print.ok();
    let bo = cmd("launchctl", &["bootout", &format!("system/{label}")]);
    let again = cmd("launchctl", &["bootstrap", "system", &path.to_string_lossy()]);
    let bo2 = cmd("launchctl", &["bootout", &format!("system/{label}")]);
    let snap_end = btm_snapshot(ctx, "B5.3-end", &needles);
    let bo_ctl = cmd("launchctl", &["bootout", &format!("system/{ctl_label}")]);
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(&ctl_path);
    for (what, o) in [("bootout", &bo), ("control bootout", &bo_ctl)] {
        if !o.ok() {
            r.anomaly(format!("{what}: {}", o.both().trim()));
        }
    }
    if again.ok() && !bo2.ok() {
        r.anomaly(format!("second bootout: {}", bo2.both().trim()));
    }
    if ino_before.is_some() && ino_before == ino_after {
        r.anomaly("rewrite kept the inode: not a fresh inode");
    }
    // The rewrite's effect: bootstrap -> rewrite. The control shows what happens with no rewrite.
    let changed = btm_changed(&snap_boot, &snap_rewrite);
    let changed_since_pre = btm_changed(&snap_pre, &snap_rewrite);
    r.differs = Some(!still_loaded || changed == Some(true) || !again.ok());
    r.observed = json!({
        "print_disabled_has_label": disabled.stdout.contains(&label), "inode_before": ino_before, "inode_after": ino_after,
        "btm_0_before_bootstrap": snap_pre, "btm_1_after_bootstrap": snap_boot, "btm_2_after_rewrite": snap_rewrite,
        "btm_3_end": snap_end, "btm_changed_bootstrap_to_rewrite": changed, "btm_changed_before_bootstrap_to_rewrite": changed_since_pre,
        "still_loaded": still_loaded, "print_after_rewrite": launch::print_summary(&print.stdout),
        "bootstrap_again_accepted": again.ok(), "bootstrap_again": again.to_json(),
        "note": "mentions of the control label (b5ctl) growing between snapshots with no rewrite means BTM lags, not the rewrite",
    });
    ctx.emit(r);
}

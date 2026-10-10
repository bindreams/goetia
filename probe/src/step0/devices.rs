//! Group D: device nodes as a launchd log or cwd. One row per step so a stall costs one row.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::common::*;
use super::launch::{self, JobSpec};

#[derive(Clone, Copy, PartialEq)]
enum Use {
    Log,
    Cwd,
}

struct Row {
    id: String,
    path: String,
    usage: Use,
    user: &'static str,
    expect: Option<&'static str>,
    /// `unavailable` when the node is absent.
    optional: bool,
}

fn row(id: &str, path: &str, usage: Use, user: &'static str, expect: Option<&'static str>) -> Row {
    Row {
        id: id.into(),
        path: path.into(),
        usage,
        user,
        expect,
        optional: false,
    }
}

fn rows() -> Vec<Row> {
    let mut v = vec![
        row("D01", "/dev/null", Use::Log, "nobody", Some("ran")),
        row("D02", "/dev/zero", Use::Log, "nobody", Some("ran")),
        row("D03", "/dev/tty", Use::Log, "nobody", Some("refused")),
        row("D04", "/dev/stdout", Use::Log, "nobody", Some("ran")),
        row("D05", "/dev/stdin", Use::Log, "nobody", Some("refused")),
        row("D06", "/dev/stderr", Use::Log, "nobody", Some("refused")),
    ];
    for n in 0..10 {
        let expect = match n {
            1 => Some("ran"),
            0 | 2 => Some("refused"),
            _ => None,
        };
        v.push(row(
            &format!("D{:02}", 7 + n),
            &format!("/dev/fd/{n}"),
            Use::Log,
            "nobody",
            expect,
        ));
    }
    v.extend([
        row("D17", "/dev/klog", Use::Log, "nobody", None),
        row("D18", "/dev/klog", Use::Log, "root", None),
        row("D19", "/dev/ptmx", Use::Log, "nobody", None),
        row("D20", "/dev/ptmx", Use::Log, "root", None),
        row("D21", "/dev/random", Use::Log, "nobody", None),
        row("D22", "/dev/urandom", Use::Log, "nobody", None),
        row("D23", "/dev/console", Use::Log, "nobody", None),
        row("D24", "/dev/console", Use::Log, "root", None),
        Row {
            optional: true,
            ..row("D25", "/dev/bpf0", Use::Log, "nobody", None)
        },
        Row {
            optional: true,
            ..row("D26", "/dev/bpf0", Use::Log, "root", None)
        },
        row("D27", "(set below)", Use::Log, "nobody", Some("refused")),
        row("D28", "(set below)", Use::Log, "nobody", Some("refused")),
        row("D29", "/dev/fd", Use::Cwd, "nobody", Some("ran")),
        row("D30", "/dev/fd/0", Use::Cwd, "nobody", Some("refused")),
    ]);
    v
}

/// D27: a raw disk node from an unmounted image.
fn disk_node(ctx: &Ctx, anomalies: &mut Vec<String>) -> Option<(String, PathBuf)> {
    let img = PathBuf::from(env_or("RUNNER_TEMP", "/tmp")).join(format!("goetia-probe-{}-d27.dmg", ctx.run));
    ctx.state("image", &img.to_string_lossy());
    must(
        anomalies,
        "hdiutil",
        &[
            "create",
            "-size",
            "4m",
            "-fs",
            "HFS+",
            "-volname",
            "GPD27",
            &img.to_string_lossy(),
        ],
    );
    let a = must(anomalies, "hdiutil", &["attach", "-nomount", &img.to_string_lossy()]);
    let node = a.stdout.lines().next()?.split_whitespace().next()?.to_string();
    ctx.state("disk", &node);
    Some((node, img))
}

/// D28: a `mknod` copy of `/dev/null` on a bare-filesystem image mounted `nodev`.
fn nodev_node(ctx: &Ctx, anomalies: &mut Vec<String>) -> Option<(String, PathBuf, Value)> {
    let img = PathBuf::from(env_or("RUNNER_TEMP", "/tmp")).join(format!("goetia-probe-{}-d28.dmg", ctx.run));
    let mnt = PathBuf::from(format!("/private/var/goetia-probe-d28-{}", ctx.run));
    ctx.state("image", &img.to_string_lossy());
    if let Err(e) = scratch_dir(ctx, &mnt) {
        anomalies.push(e);
        return None;
    }
    // `-layout NONE`: the image is one bare HFS+ volume, so the disk node is the filesystem.
    must(
        anomalies,
        "hdiutil",
        &[
            "create",
            "-size",
            "8m",
            "-layout",
            "NONE",
            "-fs",
            "HFS+",
            "-volname",
            "GPD28",
            &img.to_string_lossy(),
        ],
    );
    let a = must(anomalies, "hdiutil", &["attach", "-nomount", &img.to_string_lossy()]);
    let disk = a.stdout.lines().next()?.split_whitespace().next()?.to_string();
    ctx.state("disk", &disk);
    ctx.state("mount", &mnt.to_string_lossy());
    must(
        anomalies,
        "mount",
        &["-t", "hfs", "-o", "nodev,nosuid", &disk, &mnt.to_string_lossy()],
    );
    let rdev = cmd("stat", &["-f", "%Hr %Lr", "/dev/null"]).stdout;
    let mm: Vec<&str> = rdev.split_whitespace().collect();
    let node = mnt.join("null");
    if mm.len() == 2 {
        must(anomalies, "mknod", &[&node.to_string_lossy(), "c", mm[0], mm[1]]);
    }
    let _ = fs::set_permissions(&mnt, std::os::unix::fs::PermissionsExt::from_mode(0o755));
    let _ = fs::set_permissions(&node, std::os::unix::fs::PermissionsExt::from_mode(0o666));
    let facts = json!({ "statfs": statfs_json(&mnt), "disk": disk, "ls": cmd("ls", &["-leOd", &node.to_string_lossy()]).both() });
    let nodev = facts["statfs"]["flags_named"]
        .as_array()
        .is_some_and(|a| a.contains(&json!("NODEV")));
    if !nodev {
        anomalies.push("D28 fixture: the volume is not mounted nodev".into());
    }
    Some((node.to_string_lossy().into_owned(), mnt, facts))
}

fn detach(target: &str, anomalies: &mut Vec<String>) {
    let o = cmd("hdiutil", &["detach", target]);
    if !o.ok() {
        anomalies.push(format!("hdiutil detach {target}: {}", o.both().trim()));
    }
}

pub fn run(ctx: &mut Ctx, id: &str) {
    let Some(mut r) = rows().into_iter().find(|r| r.id == id) else {
        ctx.note(&format!("unknown device row {id}"));
        return;
    };
    let mut res = Res::new(&r.id, "D", &["decision sheet 9.1, 9.2", "A1 Q1b, literal 17"]);
    if let Some(e) = r.expect {
        res.expected = json!(e);
    }
    let mut extra = json!({});
    let mut detach_targets: Vec<String> = vec![];
    let mut umount_targets: Vec<String> = vec![];
    match id {
        "D27" => match disk_node(ctx, &mut res.anomalies) {
            Some((node, _img)) => {
                r.path = node.clone();
                detach_targets.push(node);
            }
            None => {
                res.anomaly("D27: no disk node");
                return ctx.emit(res);
            }
        },
        "D28" => match nodev_node(ctx, &mut res.anomalies) {
            Some((node, mnt, facts)) => {
                r.path = node;
                extra = facts;
                let m = mnt.to_string_lossy().into_owned();
                umount_targets.push(m);
                if let Some(d) = extra["disk"].as_str() {
                    detach_targets.push(d.to_string());
                }
            }
            None => {
                res.anomaly("D28: no nodev node");
                return ctx.emit(res);
            }
        },
        _ => {}
    }
    let path = Path::new(&r.path);
    let exists = fs::symlink_metadata(path).is_ok();
    let ls = cmd("ls", &["-leOd", &r.path]).both();
    if !exists && r.optional {
        res.verdict = "unavailable".into();
        res.observed = json!({ "path": r.path, "exists": false, "user": r.user });
        return ctx.emit(res);
    }
    ctx.provisional(&res, &r.id.to_lowercase());
    let mut spec = JobSpec::new(&r.id.to_lowercase(), r.user, Path::new("/Library/LaunchDaemons"));
    match r.usage {
        Use::Log => spec.log = Some(path.to_path_buf()),
        Use::Cwd => spec.cwd = Some(path.to_path_buf()),
    }
    let o = launch::launch(ctx, &spec);
    o.apply(&mut res);
    for t in &umount_targets {
        let o = cmd("umount", &[t]);
        if !o.ok() {
            res.anomaly(format!("umount {t}: {}", o.both().trim()));
        }
    }
    for t in &detach_targets {
        detach(t, &mut res.anomalies);
    }
    let ready = o.ready().cloned().unwrap_or(Value::Null);
    res.observed = json!({
        "path": r.path, "use": if r.usage == Use::Log { "log" } else { "cwd" }, "user": r.user, "exists": exists, "ls": ls,
        "held_fds": ready["held"], "fd1": ready["fd1"], "fd2": ready["fd2"], "w1": ready["w1"], "w2": ready["w2"],
        "extra": extra, "detail": o.detail(),
    });
    res.differs = r.expect.map(|e| e != o.verdict);
    if id != "D01" {
        let ctl = fs::read_to_string(ctx.results.join("D01.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok());
        if ctl.as_ref().and_then(|c| c["verdict"].as_str()) != Some("ran") {
            res.anomaly("control D01 missing or not Ran: this group's results are not trusted");
        }
    } else if o.verdict != "ran" {
        res.anomaly("control D01 (/dev/null) is not Ran: this group's results are not trusted");
    }
    ctx.emit(res);
}

// Pass 2, M1: the /dev/ptmx stall with O_NONBLOCK on fds 1 and 2 -----------------------------------

/// A write result as the sentinel reports it: `"<return>,<errno>"`.
fn write_result(s: &Value) -> Option<(i64, i64)> {
    let (r, e) = s.as_str()?.split_once(',')?;
    Some((r.parse().ok()?, e.parse().ok()?))
}

/// `which`: `control` (`/dev/null`, nobody), `nobody` or `root` (`/dev/ptmx`). The sentinel sets
/// `O_NONBLOCK` on fds 1 and 2, writes the nonce to each, then reports and blocks on CTRL. If the
/// stall is not the first write, the sentinel never reports and the step's timeout is the bound;
/// the provisional record then says so.
pub fn ptmx(ctx: &mut Ctx, which: &str) {
    let (user, path) = match which {
        "control" => ("nobody", "/dev/null"),
        "nobody" => ("nobody", "/dev/ptmx"),
        "root" => ("root", "/dev/ptmx"),
        other => {
            ctx.note(&format!("unknown ptmx row {other:?}"));
            return;
        }
    };
    let expect = if which == "control" {
        json!("both first writes succeed with O_NONBLOCK set")
    } else {
        json!("both first writes return -1 with EAGAIN")
    };
    let mut res = Res::new(&format!("P2.M1.{which}"), "P2", &["A1 round 10 M1 ptmx-nonblock"]).expect(expect);
    let ls = cmd("ls", &["-leOd", path]).both();
    ctx.provisional(&res, &format!("m1-{which}"));
    let mut spec = JobSpec::new(&format!("m1-{which}"), user, Path::new("/Library/LaunchDaemons"));
    spec.log = Some(PathBuf::from(path));
    spec.nonblock_stdio = true;
    let o = launch::launch(ctx, &spec);
    o.apply(&mut res);
    let rd = o.ready().cloned().unwrap_or(Value::Null);
    let (w1, w2) = (write_result(&rd["w1"]), write_result(&rd["w2"]));
    let flag = |k: &str| rd[k].as_str().and_then(|s| s.parse::<i64>().ok());
    let nonblock_set = flag("flafter1").is_some_and(|f| f & 4 != 0) && flag("flafter2").is_some_and(|f| f & 4 != 0);
    let eagain = |w: Option<(i64, i64)>| w == Some((-1, i64::from(libc::EAGAIN)));
    if which == "control" {
        let ok = |w: Option<(i64, i64)>| w.is_some_and(|(r, e)| r > 0 && e == 0);
        if o.verdict != "ran" || !ok(w1) || !ok(w2) || !nonblock_set {
            res.anomaly("control: /dev/null with --nonblock-stdio must run, set O_NONBLOCK and write both nonces");
        }
    } else {
        res.differs = Some(!(eagain(w1) && eagain(w2)));
    }
    res.observed = json!({
        "user": user, "path": path, "ls": ls, "w1": rd["w1"], "w2": rd["w2"],
        "nbset1": rd["nbset1"], "nbset2": rd["nbset2"],
        "getfl_before": { "fd1": rd["fd1"]["getfl"], "fd2": rd["fd2"]["getfl"] },
        "getfl_after": { "fd1": rd["flafter1"], "fd2": rd["flafter2"] },
        "nonblock_set": nonblock_set, "held_fds": rd["held"], "detail": o.detail(),
    });
    ctx.emit(res);
}

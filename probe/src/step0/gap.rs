//! Group Q: the known gap (an absent log under inheritable-deny parents) and stderr sharing.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::common::*;
use super::launch::{self, JobSpec};
use crate::sys;

const CASES: [(&str, &str, &str); 3] = [
    ("Q1a", "deny append,file_inherit", "recorded"),
    ("Q1b", "deny write,append,file_inherit,only_inherit", "recorded"),
    ("Q1c", "deny write,append,file_inherit", "refused"),
];

fn root(ctx: &Ctx) -> PathBuf {
    PathBuf::from(format!("/private/var/goetia-probe-gap-{}", ctx.run))
}

fn ls(p: &Path) -> String {
    cmd("ls", &["-leOd", &p.to_string_lossy()]).both()
}

/// One parent, `nobody`-owned 0755, with an ACE naming `nobody`; an absent `out.log` in it.
fn q1(ctx: &mut Ctx, id: &str, acl: &str, expect: &str) {
    let mut r = Res::new(id, "Q", &["decision sheet 15, 16", "A2-lite's baseline"]);
    let nb = sys::getpwnam("nobody").expect("nobody");
    let parent = root(ctx).join(id);
    if let Err(e) = mkdir_mode(&parent, nb.uid, nb.gid, 0o755) {
        r.anomaly(e);
        return ctx.emit(r);
    }
    let ace = format!("user:nobody {acl}");
    must(&mut r.anomalies, "chmod", &["+a", &ace, &parent.to_string_lossy()]);
    let log = parent.join("out.log");
    let before = ls(&parent);
    let log2 = log.clone();
    let created = move || json!({ "log_ls": ls(&log2) });
    let mut spec = JobSpec::new(&id.to_lowercase(), "nobody", Path::new("/Library/LaunchDaemons"));
    spec.log = Some(log.clone());
    spec.second = true;
    spec.on_ready = Some(&created);
    let o = launch::launch(ctx, &spec);
    o.apply(&mut r);
    let second = o
        .again
        .as_ref()
        .map(|a| json!({ "verdict": a.verdict, "exit_source": a.exit_source, "first_print": a.first_print }));
    let second_verdict = o.again.as_ref().map(|a| a.verdict.clone());
    if id == "Q1c" && o.verdict != "refused" {
        r.anomaly(format!("control Q1c: first launch is {}, want refused", o.verdict));
    }
    r.expected = json!({ "first": expect, "second": "refused (later launches fail EX_CONFIG)" });
    r.differs = Some(if expect == "refused" {
        o.verdict != "refused"
    } else {
        second_verdict.as_deref() == Some("ran")
    });
    r.observed = json!({
        "acl": acl, "parent_ls": before, "first": o.verdict, "second": second, "log_ls_while_running": o.first.as_ref().and_then(|f| f.on_ready.clone()),
        "log_ls_after": ls(&log), "detail": o.detail(),
    });
    ctx.emit(r);
}

/// Q2: does fd 2 share fd 1's open file description? Read from the ready line of a clean launch.
fn q2(ctx: &mut Ctx) {
    let mut r = Res::new("Q2", "Q", &["decision sheet 15", "A1 known-gap reading"]).expect(json!("shared"));
    let nb = sys::getpwnam("nobody").expect("nobody");
    let parent = root(ctx).join("Q2");
    if let Err(e) = mkdir_mode(&parent, nb.uid, nb.gid, 0o755) {
        r.anomaly(e);
        return ctx.emit(r);
    }
    let mut spec = JobSpec::new("q2", "nobody", Path::new("/Library/LaunchDaemons"));
    spec.log = Some(parent.join("out.log"));
    let o = launch::launch(ctx, &spec);
    o.apply(&mut r);
    let Some(rd) = o.ready().cloned() else {
        r.anomaly("control: the clean launch did not run");
        r.observed = json!({ "detail": o.detail() });
        return ctx.emit(r);
    };
    let off = |k: &str| {
        rd[k]
            .as_str()
            .and_then(|s| s.split(',').next())
            .and_then(|n| n.parse::<i64>().ok())
    };
    let n1len = rd["n1len"].as_str().and_then(|s| s.parse::<i64>().ok());
    let (l1, l2) = (off("l1"), off("l2"));
    let f = |fd: &str, k: &str| rd[fd][k].clone();
    let same_identity =
        f("fd1", "dev") == f("fd2", "dev") && f("fd1", "ino") == f("fd2", "ino") && !f("fd1", "ino").is_null();
    let same_flags = f("fd1", "getfl") == f("fd2", "getfl");
    let shared = l2.is_some() && l2 == n1len;
    r.differs = Some(!shared);
    r.observed = json!({
        "shared_description": shared, "n1_bytes": n1len, "lseek_fd1": l1, "lseek_fd2": l2,
        "same_file_identity": same_identity, "same_getfl": same_flags, "fd1": rd["fd1"], "fd2": rd["fd2"], "iopol": rd["iopol"],
        "reading": "lseek(2) equal to the bytes written to fd 1 means one shared open file description",
    });
    ctx.emit(r);
}

pub fn run(ctx: &mut Ctx) {
    let rt = root(ctx);
    if let Err(e) = scratch_dir(ctx, &rt) {
        ctx.note(&e);
        return;
    }
    // Q2 first: it decides how Q1's first launch should be read.
    q2(ctx);
    for (id, acl, expect) in CASES {
        q1(ctx, id, acl, expect);
    }
    let _ = Value::Null;
}

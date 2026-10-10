//! Pass 2, M4: which of goetia's steps `(deny file* (subpath D))` denies after
//! `sandbox_init(sbpl, 0)`. Process-local: the sandbox is applied to a child that exits; no
//! launchd, no mount, no system change.

use std::ffi::CStr;
use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use super::common::*;
use crate::sys;

/// `O_EXEC | O_DIRECTORY`, as pass 1's `search-stat` helper.
const O_SEARCH: i32 = 0x4000_0000 | 0x0010_0000;
/// Dropped from the denied set's expectation: the review's macOS 27 measurement.
const EXPECTED_DENIED: [&str; 1] = ["open(T, O_SEARCH)"];
const ENOTDIR: i64 = 20;

fn rc(r: i32) -> i64 {
    if r >= 0 {
        0
    } else {
        i64::from(sys::errno())
    }
}

fn step(out: &mut Vec<Value>, target: &str, name: &str, result: i64) {
    out.push(
        json!({ "target": target, "step": name, "result": if result == 0 { json!("ok") } else { json!(result) } }),
    );
}

fn open_search(p: &str) -> i32 {
    unsafe { libc::open(sys::cstr(p).as_ptr(), O_SEARCH) }
}

/// Runs inside the sandbox child (as the account it was started as). Prints one JSON line.
pub fn child(dir: &str) -> i32 {
    if let Some(why) = guard_failure() {
        eprintln!("refusing to run: {why}");
        return EXIT_GUARD;
    }
    let dir = Path::new(dir);
    let (d, c) = (dir.join("D"), dir.join("C"));
    for t in [&d, &c] {
        let made = fs::create_dir(t)
            .and_then(|_| fs::write(t.join("f"), b"x"))
            .and_then(|_| std::os::unix::fs::symlink("f", t.join("l")))
            .and_then(|_| fs::create_dir(t.join("d")))
            .and_then(|_| fs::set_permissions(t, std::os::unix::fs::PermissionsExt::from_mode(0o755)));
        if let Err(e) = made {
            eprintln!("fixture {}: {e}", t.display());
            return 3;
        }
    }
    let targets = [
        ("D", d.to_string_lossy().into_owned()),
        ("C", c.to_string_lossy().into_owned()),
    ];
    // Directory fds opened before the sandbox, and a fd to `/` to come back to after a `chdir`.
    let root_fd = unsafe { libc::open(sys::cstr("/").as_ptr(), libc::O_RDONLY) };
    let before: Vec<(i32, i64)> = targets
        .iter()
        .map(|(_, p)| {
            let fd = open_search(p);
            (fd, rc(fd))
        })
        .collect();

    let profile = sys::cstr(&format!(
        "(version 1) (allow default) (deny file* (subpath \"{}\"))",
        targets[0].1
    ));
    let mut errbuf: *mut libc::c_char = std::ptr::null_mut();
    let ret = unsafe { sys::sandbox_init(profile.as_ptr(), 0, &mut errbuf) };
    let errtext = if errbuf.is_null() {
        Value::Null
    } else {
        let t = unsafe { CStr::from_ptr(errbuf) }.to_string_lossy().into_owned();
        unsafe { sys::sandbox_free_error(errbuf) };
        json!(t)
    };

    let mut steps = vec![];
    for (i, (name, p)) in targets.iter().enumerate() {
        let cp = sys::cstr(p);
        let (dirfd_before, dirfd_before_errno) = before[i];
        step(&mut steps, name, "chdir(T)", {
            let r = rc(unsafe { libc::chdir(cp.as_ptr()) });
            if r == 0 {
                unsafe { libc::fchdir(root_fd) };
            }
            r
        });
        step(&mut steps, name, "pthread_chdir_np(T)", {
            let r = rc(unsafe { sys::pthread_chdir_np(cp.as_ptr()) });
            if r == 0 {
                unsafe { sys::pthread_fchdir_np(-1) };
            }
            r
        });
        for (label, mode) in [
            ("faccessat(T, F_OK)", libc::F_OK),
            ("faccessat(T, R_OK|W_OK)", libc::R_OK | libc::W_OK),
            ("faccessat(T, _READ_OK|_APPEND_OK)", sys::R | sys::A),
        ] {
            step(
                &mut steps,
                name,
                label,
                rc(unsafe { libc::faccessat(libc::AT_FDCWD, cp.as_ptr(), mode, 0) }),
            );
        }
        let after = open_search(p);
        step(&mut steps, name, "open(T, O_SEARCH)", rc(after));
        for (when, fd, fd_errno) in [
            ("dirfd opened before the sandbox", dirfd_before, dirfd_before_errno),
            (
                "dirfd opened after the sandbox",
                after,
                if after >= 0 { 0 } else { rc(after) },
            ),
        ] {
            for (child_name, flags, label) in [
                ("f", O_SEARCH, "openat(dirfd, f, O_SEARCH)"),
                ("d", O_SEARCH, "openat(dirfd, d, O_SEARCH)"),
                ("f", libc::O_RDONLY, "openat(dirfd, f, O_RDONLY)"),
            ] {
                let full = format!("{label} [{when}]");
                if fd < 0 {
                    step(
                        &mut steps,
                        name,
                        &format!("{full} (no dirfd, open errno {fd_errno})"),
                        -1,
                    );
                    continue;
                }
                let r = unsafe { libc::openat(fd, sys::cstr(child_name).as_ptr(), flags) };
                let e = rc(r);
                if r >= 0 {
                    unsafe { libc::close(r) };
                }
                step(&mut steps, name, &full, e);
            }
        }
        for (child_name, label) in [("f", "fstatat(T/f, NOFOLLOW)"), ("l", "fstatat(T/l, NOFOLLOW)")] {
            let full = sys::cstr(&format!("{p}/{child_name}"));
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            let r = unsafe { libc::fstatat(libc::AT_FDCWD, full.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
            step(&mut steps, name, label, rc(r));
        }
        let full = sys::cstr(&format!("{p}/l"));
        let mut buf = [0 as libc::c_char; 64];
        let r = unsafe { libc::readlinkat(libc::AT_FDCWD, full.as_ptr(), buf.as_mut_ptr(), buf.len()) };
        step(
            &mut steps,
            name,
            "readlinkat(T/l)",
            if r >= 0 { 0 } else { i64::from(sys::errno()) },
        );
    }
    println!(
        "{}",
        json!({
            "uid": unsafe { libc::getuid() }, "sandbox_init": { "ret": ret, "errbuf": errtext },
            "dirfd_before": { "D": before[0].1, "C": before[1].1 }, "steps": steps,
        })
    );
    0
}

/// Per step: denied when D's result is an error that C's is not.
fn denied(steps: &[Value]) -> (Vec<String>, Vec<String>) {
    let mut denied = vec![];
    let mut untrusted = vec![];
    let result = |t: &str, s: &str| {
        steps
            .iter()
            .find(|x| x["target"] == t && x["step"] == s)
            .map(|x| x["result"].clone())
    };
    for s in steps.iter().filter(|x| x["target"] == "D") {
        let name = s["step"].as_str().unwrap_or("");
        let (dr, cr) = (s["result"].clone(), result("C", name).unwrap_or(Value::Null));
        // `f` opened with O_SEARCH is ENOTDIR by nature, on the control too.
        let c_natural = cr == json!("ok") || (name.contains(", f, O_SEARCH)") && cr == json!(ENOTDIR));
        let c_natural = c_natural || name.contains("(no dirfd");
        if !c_natural {
            untrusted.push(format!("{name}: control result {cr}"));
        }
        if dr != cr {
            denied.push(name.to_string());
        }
    }
    (denied, untrusted)
}

pub fn run(ctx: &mut Ctx) {
    let base = std::path::PathBuf::from(format!("/private/var/goetia-probe-m4-{}", ctx.run));
    if let Err(e) = scratch_dir(ctx, &base) {
        ctx.note(&e);
        return;
    }
    let bin = ctx.base().join("bin/launchd-probe");
    let nb = sys::getpwnam("nobody").expect("nobody");
    for user in ["root", "nobody"] {
        let mut res = Res::new(&format!("P2.M4.{user}"), "P2", &["A1 round 10 M4 sandbox-steps"])
            .expect(json!({ "denied": EXPECTED_DENIED }));
        let dir = base.join(user);
        let (uid, gid) = if user == "root" { (0, 0) } else { (nb.uid, nb.gid) };
        if let Err(e) = mkdir_mode(&dir, uid, gid, 0o755) {
            res.anomaly(e);
            ctx.emit(res);
            continue;
        }
        let d = dir.to_string_lossy().into_owned();
        let b = bin.to_string_lossy().into_owned();
        let o = if user == "root" {
            cmd(&b, &["step0", "sandbox-child", &d])
        } else {
            let envs: Vec<String> = ["GITHUB_ACTIONS", "RUNNER_OS", "RUNNER_ENVIRONMENT", "PROBE_RUN"]
                .iter()
                .map(|k| format!("{k}={}", env_or(k, "")))
                .collect();
            let mut a: Vec<&str> = envs.iter().map(String::as_str).collect();
            a.extend([b.as_str(), "step0", "sandbox-child", d.as_str()]);
            as_user(user, "/usr/bin/env", &a)
        };
        let doc: Option<Value> = o.stdout.lines().last().and_then(|l| serde_json::from_str(l).ok());
        let Some(doc) = doc.filter(|_| o.ok()) else {
            res.anomaly(format!("sandbox child as {user}: {:?} {}", o.code, o.both().trim()));
            res.observed = json!({ "child": o.to_json() });
            ctx.emit(res);
            continue;
        };
        let steps = doc["steps"].as_array().cloned().unwrap_or_default();
        let (den, untrusted) = denied(&steps);
        for u in &untrusted {
            res.anomaly(format!("control C is not clean, the row is untrusted: {u}"));
        }
        let sandbox_ok = doc["sandbox_init"]["ret"] == json!(0);
        let want: Vec<String> = EXPECTED_DENIED.iter().map(|s| s.to_string()).collect();
        res.differs = Some(!sandbox_ok || den != want);
        res.observed = json!({
            "user": user, "sandbox_init": doc["sandbox_init"], "dirfd_before": doc["dirfd_before"],
            "denied_steps": den, "steps": steps,
            "note": "denied = D's result is an error that C's is not; `ok` otherwise",
        });
        ctx.emit(res);
    }
}

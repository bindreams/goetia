//! Group T: TCC-protected folders as a launchd cwd or log, for accounts without Full Disk Access.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::common::*;
use super::launch::{self, JobSpec};
use crate::sys::Pw;

/// `(step key, path under the home, plan expectation)`.
pub const DIRS: [(&str, &str, Option<&str>); 13] = [
    ("home", "", Some("ran")),
    ("public", "Public", Some("ran")),
    ("desktop", "Desktop", Some("refused")),
    ("documents", "Documents", Some("refused")),
    ("downloads", "Downloads", Some("refused")),
    ("pictures", "Pictures", Some("refused")),
    ("mobile-documents", "Library/Mobile Documents", Some("refused")),
    ("containers", "Library/Containers", Some("refused")),
    ("group-containers", "Library/Group Containers", None),
    ("developer", "Developer", None),
    ("mail", "Library/Mail", None),
    ("messages", "Library/Messages", None),
    ("safari", "Library/Safari", None),
];

fn home(ctx: &Ctx, n: u32) -> PathBuf {
    if n == 1 {
        PathBuf::from(format!("/Users/_gtcc1-{}", ctx.run))
    } else {
        PathBuf::from(format!("/private/var/goetia-test-home-{}", ctx.run))
    }
}

fn accounts(ctx: &Ctx) -> Vec<(u32, Option<Pw>)> {
    (1..=2)
        .map(|n| (n, crate::sys::getpwnam(&format!("_gtcc{n}-{}", ctx.run))))
        .collect()
}

/// Two fresh accounts, their homes and every directory, then T1's representativeness record.
pub fn setup(ctx: &mut Ctx) {
    let mut r = Res::new("T1.setup", "T", &["decision sheet 11", "A1 Q9b (ii)"]);
    let mut an = vec![];
    for n in 1..=2u32 {
        let h = home(ctx, n);
        let Some(pw) = create_account(ctx, &format!("tcc{n}"), Some(&h), &mut an) else {
            continue;
        };
        for (_, rel, _) in DIRS {
            let mut cur = h.clone();
            for part in Path::new(rel).components() {
                cur.push(part);
                if let Err(e) = mkdir_mode(&cur, pw.uid, pw.gid, 0o755) {
                    an.push(e);
                }
            }
        }
    }
    r.anomalies.extend(an);
    let db = "/Library/Application Support/com.apple.TCC/TCC.db";
    let dump = cmd(
        "sqlite3",
        &["-readonly", db, "select service, client, auth_value from access"],
    );
    r.observed = json!({
        "csrutil": sh("csrutil status").trim(),
        "system_tcc_db": { "code": dump.code, "rows": dump.stdout.lines().collect::<Vec<_>>(), "error": dump.stderr.trim() },
        "note": "runners have SIP off and a modified TCC database: evidence about CI, not about production hosts",
    });
    ctx.emit(r);
}

fn one(ctx: &mut Ctx, n: u32, pw: &Pw, key: &str, rel: &str, expect: Option<&'static str>) {
    let dir = if rel.is_empty() {
        home(ctx, n)
    } else {
        home(ctx, n).join(rel)
    };
    for usage in ["cwd", "log"] {
        let id = format!("T1.{}.{key}.{usage}", if n == 1 { "users" } else { "outside" });
        let mut r = Res::new(&id, "T", &["decision sheet 11", "A1 Q9b (ii)"]);
        if let Some(e) = expect {
            r.expected = json!(e);
        }
        let mut spec = JobSpec::new(
            &format!("t{n}-{key}-{usage}"),
            &pw.name,
            Path::new("/Library/LaunchDaemons"),
        );
        if usage == "cwd" {
            spec.cwd = Some(dir.clone());
        } else {
            spec.log = Some(dir.join("probe.log"));
        }
        let o = launch::launch(ctx, &spec);
        o.apply(&mut r);
        r.differs = expect.map(|e| e != o.verdict);
        r.observed = json!({ "account": pw_json(pw), "dir": dir, "use": usage, "ls": cmd("ls", &["-leOd@", &dir.to_string_lossy()]).both(), "detail": o.detail() });
        ctx.emit(r);
    }
}

pub fn dir_step(ctx: &mut Ctx, key: &str) {
    let Some((_, rel, expect)) = DIRS.iter().find(|(k, _, _)| *k == key).copied() else {
        return ctx.note(&format!("unknown TCC dir key {key}"));
    };
    for (n, pw) in accounts(ctx) {
        match pw {
            Some(pw) => one(ctx, n, &pw, key, rel, expect),
            None => ctx.note(&format!("account {n} missing: did tcc-setup run?")),
        }
    }
}

/// T-removable: the log on a disk image mounted under /Volumes, as `nobody`.
pub fn removable(ctx: &mut Ctx) {
    let mut r = Res::new("T1.removable", "T", &["decision sheet 11"]);
    let img = PathBuf::from(env_or("RUNNER_TEMP", "/tmp")).join(format!("goetia-probe-{}-tcc.dmg", ctx.run));
    let mnt = PathBuf::from(format!("/Volumes/goetia-probe-{}", ctx.run));
    ctx.state("image", &img.to_string_lossy());
    must(
        &mut r.anomalies,
        "hdiutil",
        &[
            "create",
            "-size",
            "16m",
            "-fs",
            "HFS+",
            "-volname",
            "GPTCC",
            &img.to_string_lossy(),
        ],
    );
    ctx.state("mount", &mnt.to_string_lossy());
    let a = must(
        &mut r.anomalies,
        "hdiutil",
        &[
            "attach",
            "-nobrowse",
            "-owners",
            "on",
            "-mountpoint",
            &mnt.to_string_lossy(),
            &img.to_string_lossy(),
        ],
    );
    if !a.ok() {
        return ctx.emit(r);
    }
    let _ = std::fs::set_permissions(&mnt, std::os::unix::fs::PermissionsExt::from_mode(0o777));
    let fs_info = statfs_json(&mnt);
    let mut spec = JobSpec::new("t-removable", "nobody", Path::new("/Library/LaunchDaemons"));
    spec.log = Some(mnt.join("probe.log"));
    let o = launch::launch(ctx, &spec);
    o.apply(&mut r);
    let d = cmd("hdiutil", &["detach", &mnt.to_string_lossy()]);
    if !d.ok() {
        r.anomaly(format!("hdiutil detach: {}", d.both().trim()));
    }
    let removable = fs_info["flags_named"]
        .as_array()
        .map(|a| a.contains(&json!("REMOVABLE")));
    r.observed = json!({ "statfs": fs_info, "mnt_removable": removable, "detail": o.detail() });
    ctx.emit(r);
}

/// T2: one lookup in `~/Documents` as `_gtcc1`, then the TCC subsystem's log. Never gating.
pub fn t2(ctx: &mut Ctx) {
    let mut r = Res::new("T2", "T", &["decision sheet 9.3", "A1 Q9"]).expect(json!("positive or inconclusive"));
    let Some(pw) = accounts(ctx).into_iter().find(|(n, _)| *n == 1).and_then(|(_, p)| p) else {
        r.anomaly("account _gtcc1 missing: did tcc-setup run?");
        return ctx.emit(r);
    };
    let docs = home(ctx, 1).join("Documents");
    let ts = sh("date '+%Y-%m-%d %H:%M:%S'").trim().to_string();
    let bin = ctx.base().join("bin/launchd-probe");
    let child = as_user(
        &pw.name,
        &bin.to_string_lossy(),
        &["step0", "helper", "lookup", &docs.to_string_lossy()],
    );
    let pid = child
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("pid="))
        .map(str::to_string);
    let log = cmd(
        "log",
        &[
            "show",
            "--info",
            "--style",
            "compact",
            "--predicate",
            "subsystem == \"com.apple.TCC\"",
            "--start",
            &ts,
        ],
    );
    let all: Vec<&str> = log.stdout.lines().collect();
    let matched: Vec<&str> = match &pid {
        Some(p) => all
            .iter()
            .copied()
            .filter(|l| l.split(|c: char| !c.is_ascii_digit()).any(|w| w == p))
            .collect(),
        None => vec![],
    };
    r.verdict = if matched.is_empty() {
        "inconclusive".into()
    } else {
        "value".into()
    };
    r.observed = json!({
        "child": child.to_json(), "child_pid": pid, "start": ts, "log_code": log.code, "tcc_lines_total": all.len(),
        "tcc_lines_for_pid": matched.iter().take(50).collect::<Vec<_>>(), "matched": matched.len(),
        "note": "a negative result is inconclusive: info-level entries may not be persisted yet",
    });
    if pid.is_none() {
        r.anomaly("the lookup child printed no pid");
    }
    ctx.emit(r);
    let _ = Value::Null;
}

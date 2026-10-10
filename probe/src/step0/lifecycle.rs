//! Pass 2, M2 and M2b: the suspended-start lifecycle against refused and ran fixtures, and a stale
//! `last exit code` on one label. One result per fixture, holding every launch.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::common::*;
use super::launch::JobSpec;
use super::suspended::{Launch, Session};
use crate::sys;

const QUESTION: &[&str] = &["A1 round 10 M2 suspended-start"];
const QUESTION_B: &[&str] = &["A1 round 10 M2b stale-exit-code"];
const LD: &str = "/Library/LaunchDaemons";

/// Launch counts: pass 1's X1 count for the refused fixtures, 10 for the control.
fn count(fixture: &str) -> usize {
    if fixture == "control" {
        10
    } else {
        33
    }
}

fn root(ctx: &Ctx) -> PathBuf {
    PathBuf::from(format!("/private/var/goetia-probe-m2-{}", ctx.run))
}

fn launch_json(i: usize, suffix: &str, l: &Launch) -> Value {
    json!({ "i": i, "suffix": suffix, "verdict": l.verdict, "record": l.record, "anomalies": l.anomalies })
}

/// The anomalies that make a launch a harness failure rather than a measurement.
fn harness_anomalies(res: &mut Res, tag: &str, s: &Session, l: &Launch) {
    for a in &l.anomalies {
        res.anomaly(format!("{tag}: {a}"));
    }
    for a in &s.anomalies {
        res.anomaly(format!("{tag}: {a}"));
    }
    if l.verdict == "bootstrap-refused" {
        res.anomaly(format!("{tag}: bootstrap refused: {}", s.bootstrap));
    }
}

/// Builds one launch's spec for `fixture`, creating its directories.
fn spec_for<'a>(ctx: &Ctx, fixture: &str, i: usize, suffix: &str, an: &mut Vec<String>) -> JobSpec<'a> {
    let r = root(ctx);
    let mut spec = JobSpec::new(suffix, "nobody", Path::new(LD));
    let nb = sys::getpwnam("nobody").expect("nobody");
    match fixture {
        "cwd" => {
            let parent = r.join(format!("cwd-{i:02}"));
            if let Err(e) = mkdir_mode(&parent, 0, 0, 0o755) {
                an.push(e);
            }
            spec.cwd = Some(parent.join("missing"));
        }
        "log" => {
            let dir = r.join("rootonly");
            if let Err(e) = mkdir_mode(&dir, 0, 0, 0o700) {
                an.push(e);
            }
            spec.log = Some(dir.join(format!("out-{i:02}.log")));
        }
        "gap" => {
            let parent = r.join(format!("gap-{i:02}"));
            if let Err(e) = mkdir_mode(&parent, nb.uid, nb.gid, 0o755) {
                an.push(e);
            }
            must(
                an,
                "chmod",
                &["+a", "user:nobody deny append,file_inherit", &parent.to_string_lossy()],
            );
            spec.log = Some(parent.join("out.log"));
        }
        _ => {
            let dir = r.join(format!("ctl-{i:02}"));
            if let Err(e) = mkdir_mode(&dir, nb.uid, nb.gid, 0o755) {
                an.push(e);
            }
            spec.cwd = Some(dir.clone());
            spec.log = Some(dir.join("out.log"));
        }
    }
    spec
}

pub fn m2(ctx: &mut Ctx, fixture: &str) {
    if !["cwd", "log", "gap", "control"].contains(&fixture) {
        ctx.note(&format!("unknown M2 fixture {fixture:?}"));
        return;
    }
    let expect = if fixture == "control" { "ran" } else { "refused" };
    let mk = || {
        Res::new(&format!("P2.M2.{fixture}"), "P2", QUESTION).expect(json!({
            "verdict": expect, "receipt": 0, "exit": if expect == "ran" { "exited:0" } else { "exited:78" },
        }))
    };
    let mut res = mk();
    if let Err(e) = scratch_dir(ctx, &root(ctx)) {
        res.anomaly(e);
        return ctx.emit(res);
    }
    let phase_name = format!("m2-{fixture}");
    ctx.provisional(&res, &phase_name);
    let n = count(fixture);
    let done: RefCell<Vec<Value>> = RefCell::new(vec![]);
    for i in 1..=n {
        let suffix = format!("m2-{fixture}-{i:02}");
        let mut setup_an = vec![];
        let spec = spec_for(ctx, fixture, i, &suffix, &mut setup_an);
        for a in setup_an {
            res.anomaly(format!("launch {i}: {a}"));
        }
        let mut session = Session::open(ctx, &spec);
        let phase = |w: &str| ctx.phase(&phase_name, &format!("{i}/{n}: {w}"));
        let progress = |rec: &Value| {
            ctx.provisional_with(
                &mk(),
                &phase_name,
                json!({ "launches_done": done.borrow().clone(), "in_flight": { "i": i, "suffix": suffix, "record": rec } }),
            )
        };
        let l = session.launch(&phase, &progress);
        harness_anomalies(&mut res, &format!("launch {i}"), &session, &l);
        for a in session.close() {
            res.anomaly(format!("launch {i}: {a}"));
        }
        if fixture == "control" && l.verdict != "ran" {
            res.anomaly(format!("control launch {i} is {}, want ran", l.verdict));
        }
        done.borrow_mut().push(launch_json(i, &suffix, &l));
        ctx.provisional_with(&mk(), &phase_name, json!({ "launches_done": done.borrow().clone() }));
    }
    let launches = done.into_inner();
    res.differs = Some(launches.iter().any(|l| l["verdict"] != json!(expect)));
    res.observed = json!({ "fixture": fixture, "n": n, "launches": launches });
    ctx.emit(res);
}

/// M2b: one label, two launches of a missing cwd, the cwd repaired, a third launch; ten times.
pub fn m2b(ctx: &mut Ctx) {
    let mk = || Res::new("P2.M2b", "P2", QUESTION_B).expect(json!("per launch: print recorded before SIGCONT"));
    let mut res = mk();
    if let Err(e) = scratch_dir(ctx, &root(ctx)) {
        res.anomaly(e);
        return ctx.emit(res);
    }
    let phase_name = "m2b".to_string();
    ctx.provisional(&res, &phase_name);
    let done: RefCell<Vec<Value>> = RefCell::new(vec![]);
    const SEQUENCES: usize = 10;
    for k in 1..=SEQUENCES {
        let suffix = format!("m2b-{k:02}");
        let parent = root(ctx).join(format!("m2b-{k:02}"));
        if let Err(e) = mkdir_mode(&parent, 0, 0, 0o755) {
            res.anomaly(format!("sequence {k}: {e}"));
        }
        let cwd = parent.join("cwd");
        let mut spec = JobSpec::new(&suffix, "nobody", Path::new(LD));
        spec.cwd = Some(cwd.clone());
        let mut session = Session::open(ctx, &spec);
        let mut launches = vec![];
        let mut repair = Value::Null;
        for n in 1..=3 {
            if n == 3 {
                repair = json!(mkdir_mode(&cwd, 0, 0, 0o755).err());
            }
            let phase = |w: &str| ctx.phase(&phase_name, &format!("sequence {k}/{SEQUENCES} launch {n}: {w}"));
            let progress = |rec: &Value| {
                ctx.provisional_with(
                    &mk(),
                    &phase_name,
                    json!({ "sequences_done": done.borrow().clone(), "in_flight": { "k": k, "n": n, "record": rec } }),
                )
            };
            let l = session.launch(&phase, &progress);
            harness_anomalies(&mut res, &format!("sequence {k} launch {n}"), &session, &l);
            launches.push(launch_json(n, &suffix, &l));
        }
        for a in session.close() {
            res.anomaly(format!("sequence {k}: {a}"));
        }
        done.borrow_mut()
            .push(json!({ "k": k, "repair_error": repair, "launches": launches }));
        ctx.provisional_with(&mk(), &phase_name, json!({ "sequences_done": done.borrow().clone() }));
    }
    res.observed = json!({ "sequences": done.into_inner() });
    ctx.emit(res);
}

/// The verdict machinery through the new lifecycle: p0 is Ran, a missing cwd is Refused with 78.
pub fn selfcheck(ctx: &mut Ctx) {
    for (name, want, missing_cwd) in [("suspended-ran", "ran", false), ("suspended-refused", "refused", true)] {
        let mut r = Res::new(&format!("SELF.{}.{name}", ctx.job), "self", &[]).expect(json!(want));
        let mut spec = JobSpec::new(&format!("self-{name}"), "nobody", Path::new(LD));
        if missing_cwd {
            spec.cwd = Some(format!("/nonexistent-goetia-probe-{}", ctx.run).into());
        }
        let mut session = Session::open(ctx, &spec);
        let phase = |w: &str| ctx.phase(&format!("self-{name}"), w);
        let l = session.launch(&phase, &|_| {});
        harness_anomalies(&mut r, name, &session, &l);
        for a in session.close() {
            r.anomaly(format!("{name}: {a}"));
        }
        let exit_ok = l.record["exit"] == json!(if missing_cwd { "exited:78" } else { "exited:0" });
        if l.verdict != want || !exit_ok {
            r.anomaly(format!(
                "{name}: verdict {} exit {}, want {want}",
                l.verdict, l.record["exit"]
            ));
        }
        r.verdict = l.verdict.clone();
        r.observed = launch_json(1, name, &l);
        ctx.emit(r);
    }
}

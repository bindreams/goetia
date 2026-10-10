//! Pass 2, M3: A1 Task 1's setup steps 1 to 4 on a fresh runner, then exactly one of three mount
//! sequences, once. The workflow's matrix supplies `form` and `rep`; each cell is its own runner.
//! No wait, no retry: a refused mount is the measurement.

use std::path::Path;
use std::time::Instant;

use serde_json::{json, Value};

use super::common::*;
use super::nfs::{export, exports_line, install_exports, mnt1, record_nfsd_state, start_nfsd};

const FORMS: [&str; 3] = ["showmount", "default", "retrycnt0"];

pub fn run(ctx: &mut Ctx, form: &str, rep: &str) {
    let id = format!("P2.M3.{form}.{rep}");
    let mut res = Res::new(&id, "P2", &["A1 round 10 M3 nfs-shipped"]).expect(json!("mounts first time"));
    if !FORMS.contains(&form) || rep.parse::<u32>().is_err() {
        res.anomaly(format!("form {form:?} rep {rep:?}: want one of {FORMS:?} and a number"));
        return ctx.emit(res);
    }
    ctx.provisional(&res, &format!("m3-{form}-{rep}"));
    let (exp, mnt) = (export(ctx), mnt1(ctx));
    // 1. nfsd's prior state.
    let (enabled, running) = record_nfsd_state(ctx);
    // 2. The export (with a directory to stat, made before any lookup) and the mount point.
    for d in [&exp, &mnt, &exp.join("allowed")] {
        if let Err(e) = scratch_dir(ctx, d) {
            res.anomaly(e);
        }
    }
    // 3 and 4. /etc/exports; checkexports, enable, start or update.
    let line = exports_line(ctx, None);
    install_exports(ctx, &line, &mut res.anomalies);
    start_nfsd(ctx, enabled, running, &mut res.anomalies);

    // Exactly one of the three sequences.
    let showmount = (form == "showmount").then(|| cmd("showmount", &["-e", "127.0.0.1"]));
    let opts = if form == "retrycnt0" {
        "vers=3,inet,soft,retrycnt=0"
    } else {
        "vers=3,inet,soft"
    };
    ctx.state("mount", &mnt.to_string_lossy());
    let t0 = Instant::now();
    let mo = cmd(
        "mount",
        &[
            "-t",
            "nfs",
            "-o",
            opts,
            &format!("127.0.0.1:{}", exp.display()),
            &mnt.to_string_lossy(),
        ],
    );
    let elapsed = t0.elapsed().as_secs_f64();
    let mounted = mo.ok();
    let listed = || !sh(&format!("mount | grep -F -- '{}'", mnt.display())).trim().is_empty();
    let mount_line = sh(&format!("mount | grep -F -- '{}'", mnt.display()))
        .trim()
        .to_string();
    let nfsstat = cmd("nfsstat", &["-m"]).both();

    // One stat of an allowed directory through the mount, then the verified unmount.
    let stat = mounted.then(|| match std::fs::metadata(mnt.join("allowed")) {
        Ok(_) => json!(0),
        Err(e) => json!(e.raw_os_error()),
    });
    let mut unmount = Value::Null;
    if mounted {
        let um = cmd("umount", &[&mnt.to_string_lossy()]);
        let still = listed();
        let parent = mnt.parent().unwrap_or(Path::new("/"));
        let same_dev = st_dev(&mnt) == st_dev(parent);
        if !um.ok() || still || !same_dev {
            res.anomaly(format!(
                "unmount of {} not verified: umount {:?}, still listed {still}, same st_dev as parent {same_dev}",
                mnt.display(),
                um.both().trim()
            ));
        }
        unmount = json!({ "umount": um.to_json(), "still_listed": still, "same_st_dev_as_parent": same_dev });
    }
    res.differs = Some(!mounted);
    res.observed = json!({
        "form": form, "rep": rep, "mount_options": opts, "mounted": mounted,
        "mount": mo.to_json(), "elapsed_seconds": elapsed, "mount_line": mount_line, "nfsstat_m": nfsstat,
        "showmount": showmount.map(|s| s.to_json()), "stat_allowed_errno": stat, "unmount": unmount,
        "nfsd_enabled_before": enabled, "nfsd_running_before": running,
    });
    ctx.emit(res);
}

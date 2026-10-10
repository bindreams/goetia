//! B6: how large a plist can `launchctl bootstrap` take. Runs alone and last: a huge plist may
//! stall launchd, and the step's `timeout-minutes` is the human-facing bound.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::json;

use super::common::*;
use super::launch;

const MIB: u64 = 1 << 20;

/// A valid plist of exactly `size` bytes: the padding is an XML comment (spaces when it is too
/// short to hold one).
fn write_sized(path: &Path, label: &str, size: u64) -> Result<(), String> {
    let full = launch::minimal_plist(label);
    let at = full.find("<dict>").ok_or("no <dict>")?;
    let (head, tail) = full.split_at(at);
    let fixed = (head.len() + tail.len()) as u64;
    if size < fixed {
        return Err(format!("size {size} below the minimal plist ({fixed})"));
    }
    let pad = size - fixed;
    let mut f = fs::File::create(path).map_err(|e| format!("create: {e}"))?;
    let w = |f: &mut fs::File, b: &[u8]| f.write_all(b).map_err(|e| format!("write: {e}"));
    w(&mut f, head.as_bytes())?;
    let (open, close, fill) = if pad >= 7 {
        ("<!--", "-->", pad - 7)
    } else {
        ("", "", pad)
    };
    w(&mut f, open.as_bytes())?;
    let chunk = vec![if pad >= 7 { b'x' } else { b' ' }; MIB as usize];
    let mut left = fill;
    while left > 0 {
        let n = left.min(MIB) as usize;
        w(&mut f, &chunk[..n])?;
        left -= n as u64;
    }
    w(&mut f, close.as_bytes())?;
    w(&mut f, tail.as_bytes())?;
    f.sync_all().map_err(|e| format!("sync: {e}"))?;
    drop(f);
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).map_err(|e| e.to_string())?;
    std::os::unix::fs::chown(path, Some(0), Some(0)).map_err(|e| e.to_string())
}

struct Trial {
    accepted: bool,
}

fn trial(ctx: &mut Ctx, dir: &Path, n: usize, size: u64) -> Result<Trial, String> {
    let label = ctx.label(&format!("b6t{n}"));
    let path = dir.join(format!("{label}.plist"));
    ctx.state("label", &label);
    println!("trial {n}: {size} bytes (started)");
    write_sized(&path, &label, size)?;
    let actual = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let t = Instant::now();
    let boot = cmd("launchctl", &["bootstrap", "system", &path.to_string_lossy()]);
    let wall = t.elapsed().as_secs_f64();
    if boot.ok() {
        let bo = cmd("launchctl", &["bootout", &format!("system/{label}")]);
        if !bo.ok() {
            ctx.note(&format!("bootout {label}: {}", bo.both().trim()));
        }
    }
    let _ = fs::remove_file(&path);
    let mut r = Res::new(&format!("B6.{size}"), "B", &["B0 Q-S6 (the cap)"]).verdict(if boot.ok() {
        "value"
    } else {
        "bootstrap-refused"
    });
    r.observed = json!({ "size": size, "file_size": actual, "accepted": boot.ok(), "wall_seconds": wall, "bootstrap": boot.to_json() });
    if actual != size {
        r.anomaly(format!("file is {actual} bytes, want {size}"));
    }
    ctx.emit(r);
    Ok(Trial { accepted: boot.ok() })
}

pub fn b6(ctx: &mut Ctx) {
    let mut summary = Res::new("B6", "B", &["B0 Q-S6 (the cap)"]).expect(json!("a limit, or none"));
    let dir = PathBuf::from(format!("/private/var/goetia-probe-b6-{}", ctx.run));
    if let Err(e) = scratch_dir(ctx, &dir) {
        summary.anomaly(e);
        return ctx.emit(summary);
    }
    let min = launch::minimal_plist(&ctx.label("b6t1")).len() as u64;
    let mut n = 0usize;
    let mut next = |ctx: &mut Ctx, size: u64| -> Result<Trial, String> {
        n += 1;
        trial(ctx, &dir, n, size)
    };
    // 1. Control: the minimal valid plist is accepted. Its label has the same length as the others.
    match next(ctx, min) {
        Ok(t) if t.accepted => {}
        Ok(_) => {
            summary.anomaly("control: the minimal plist was refused");
            return ctx.emit(summary);
        }
        Err(e) => {
            summary.anomaly(e);
            return ctx.emit(summary);
        }
    }
    // 2. Ladder, ascending; stop at the first refusal.
    let mut lo = min;
    let mut hi: Option<u64> = None;
    for mib in [1u64, 4, 16, 64, 256, 1024] {
        let size = mib * MIB;
        match next(ctx, size) {
            Ok(t) if t.accepted => lo = size,
            Ok(_) => {
                hi = Some(size);
                break;
            }
            Err(e) => {
                summary.anomaly(e);
                return ctx.emit(summary);
            }
        }
    }
    // 3 and 4. Bisection when there was a refusal: the interval halves, so it ends by itself.
    let observed = match hi {
        None => json!({ "limit": null, "note": "no refusal up to 1 GiB", "largest_accepted": lo }),
        Some(mut hi) => {
            let first_refused = hi;
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                match next(ctx, mid) {
                    Ok(t) if t.accepted => lo = mid,
                    Ok(_) => hi = mid,
                    Err(e) => {
                        summary.anomaly(e);
                        break;
                    }
                }
            }
            json!({ "limit": lo, "limit_mib": lo as f64 / MIB as f64, "smallest_refused": hi, "first_refused_rung": first_refused })
        }
    };
    summary.observed = observed;
    ctx.emit(summary);
}

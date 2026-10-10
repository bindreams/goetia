//! State-driven teardown, a separate `if: always()` step. Order: boot out labelled jobs, unmount
//! and verify, restore /etc/exports and nfsd, and only if every unmount verified `rm -rf -x`
//! the scratch directories and delete the accounts. A failing unmount stops everything and
//! deletes nothing.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::common::*;

struct State(Vec<(String, String)>);

impl State {
    fn read(ctx: &Ctx) -> State {
        let text = fs::read_to_string(ctx.state_path()).unwrap_or_default();
        State(
            text.lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    fn all(&self, key: &str) -> Vec<String> {
        let mut seen = BTreeSet::new();
        self.0
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .filter(|v| seen.insert(v.clone()))
            .collect()
    }

    fn last(&self, key: &str) -> Option<&str> {
        self.0.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

fn say(msg: &str) {
    println!("teardown: {msg}");
}

fn mount_points() -> Vec<String> {
    sh("mount")
        .lines()
        .filter_map(|l| {
            let (_, rest) = l.split_once(" on ")?;
            Some(rest.rsplit_once(" (")?.0.to_string())
        })
        .collect()
}

fn spellings(p: &str) -> Vec<String> {
    let bare = p.strip_prefix("/private").unwrap_or(p).to_string();
    vec![p.to_string(), bare.clone(), format!("/private{bare}")]
}

/// Mount points at or under `dir`, in either spelling.
fn mounts_under(dir: &str) -> Vec<String> {
    let want = spellings(dir);
    mount_points()
        .into_iter()
        .filter(|m| want.iter().any(|w| m == w || m.starts_with(&format!("{w}/"))))
        .collect()
}

fn is_mounted(p: &str) -> bool {
    let want = spellings(p);
    mount_points().iter().any(|m| want.contains(m))
}

fn safe_to_delete(ctx: &Ctx, p: &str) -> bool {
    let named = p.contains(&ctx.run);
    let under = [
        "/tmp/",
        "/private/tmp/",
        "/private/var/",
        "/Library/Application Support/",
        "/Users/",
        "/Volumes/",
    ];
    named && under.iter().any(|u| p.starts_with(u)) && p.matches('/').count() >= 2
}

/// Whole-disk nodes of attached images whose path is `image`.
fn attached_disks(image: &str) -> Vec<String> {
    let info = cmd("hdiutil", &["info"]).stdout;
    let mut cur_match = false;
    let mut out = BTreeSet::new();
    for l in info.lines() {
        if let Some(v) = l.strip_prefix("image-path") {
            cur_match = v.trim_start_matches([' ', ':']).trim() == image;
        } else if cur_match && l.starts_with("/dev/disk") {
            if let Some(tok) = l.split_whitespace().next() {
                let body = tok.trim_start_matches("/dev/disk");
                let n: String = body.chars().take_while(|c| c.is_ascii_digit()).collect();
                out.insert(format!("/dev/disk{n}"));
            }
        }
    }
    out.into_iter().collect()
}

pub fn run(ctx: &Ctx) -> i32 {
    let st = State::read(ctx);
    let mut red = false;
    let prefix = format!("com.goetia.probe.{}.", ctx.run);

    // 0. Boot out every labelled job, so nothing holds a mount.
    let mut labels: BTreeSet<String> = st.all("label").into_iter().collect();
    for tok in cmd("launchctl", &["print", "system"]).stdout.split_whitespace() {
        if tok.starts_with(&prefix) {
            labels.insert(tok.trim_matches(|c: char| c == '"' || c == ',' || c == ';').to_string());
        }
    }
    for l in &labels {
        let o = cmd("launchctl", &["bootout", &format!("system/{l}")]);
        let b = o.both();
        if !o.ok() && !b.contains("No such process") && !b.contains("Could not find") && !b.contains("No such file") {
            say(&format!("bootout {l}: {}", b.trim()));
            red = true;
        }
    }
    for p in st.all("plist") {
        if p.contains(&prefix) {
            let _ = fs::remove_file(&p);
        }
    }

    // 1. Leftover children, then unmounts, then the verification.
    for pid in st.all("pid") {
        let cmdline = cmd("ps", &["-o", "command=", "-p", &pid]).stdout;
        if cmdline.contains("probe") {
            say(&format!("kill -9 {pid}: {}", cmdline.trim()));
            let _ = cmd("kill", &["-9", &pid]);
        }
    }
    let mut mounts = st.all("mount");
    let forced: BTreeSet<String> = st.all("mount-force").into_iter().collect();
    mounts.extend(forced.iter().cloned());
    mounts.reverse();
    for m in &mounts {
        if !is_mounted(m) {
            continue;
        }
        let o = if forced.contains(m) {
            cmd("umount", &["-f", m])
        } else {
            cmd("umount", &[m])
        };
        if !o.ok() {
            let d = cmd("hdiutil", &["detach", m]);
            if !d.ok() {
                say(&format!(
                    "UNMOUNT FAILED {m}: {} / {}",
                    o.both().trim(),
                    d.both().trim()
                ));
                say("stopping: nothing will be deleted");
                return 1;
            }
        }
    }
    for img in st.all("image") {
        for disk in attached_disks(&img) {
            let d = cmd("hdiutil", &["detach", &disk]);
            if !d.ok() {
                say(&format!("DETACH FAILED {disk} ({img}): {}", d.both().trim()));
                say("stopping: nothing will be deleted");
                return 1;
            }
        }
    }
    for disk in st.all("disk") {
        if Path::new(&disk).exists() {
            let d = cmd("hdiutil", &["detach", &disk]);
            if !d.ok() && Path::new(&disk).exists() && !d.both().contains("No such") {
                say(&format!("DETACH FAILED {disk}: {}", d.both().trim()));
                say("stopping: nothing will be deleted");
                return 1;
            }
        }
    }
    for m in &mounts {
        let parent = Path::new(m).parent().unwrap_or(Path::new("/"));
        if Path::new(m).exists() && st_dev(Path::new(m)) != st_dev(parent) {
            say(&format!("VERIFY FAILED: st_dev of {m} differs from its parent's"));
            return 1;
        }
    }
    let mut scratch: Vec<String> = st.all("dir");
    scratch.push(ctx.base().to_string_lossy().into_owned());
    for d in &scratch {
        let left = mounts_under(d);
        if !left.is_empty() {
            say(&format!("VERIFY FAILED: mounts under {d}: {left:?}"));
            say("stopping: nothing will be deleted");
            return 1;
        }
    }

    // 2. nfsd first (it reads /etc/exports), then /etc/exports.
    if st.last("nfsd_touched").is_some() {
        if st.last("nfsd_running_before") == Some("0") {
            let o = cmd("nfsd", &["stop"]);
            say(&format!("nfsd stop: {:?} {}", o.code, o.both().trim()));
        }
        if st.last("nfsd_enabled_before") == Some("0") {
            let o = cmd("nfsd", &["disable"]);
            say(&format!("nfsd disable: {:?} {}", o.code, o.both().trim()));
            let disabled = cmd("launchctl", &["print-disabled", "system"])
                .stdout
                .lines()
                .any(|l| l.contains("com.apple.nfsd") && l.contains("disabled") && !l.contains("enabled"));
            if !disabled {
                say("nfsd is not listed as disabled after nfsd disable");
                red = true;
            }
        }
    }
    if let Some(mode) = st.last("exports") {
        if let Err(e) = restore_exports(&st, mode) {
            say(&format!("exports: {e}"));
            red = true;
        }
        if st.last("nfsd_running_before") == Some("1") {
            let _ = cmd("nfsd", &["update"]);
        }
    }

    // 3. Only now delete: scratch trees, accounts, images.
    for d in &scratch {
        if !Path::new(d).exists() {
            continue;
        }
        if !safe_to_delete(ctx, d) {
            say(&format!("refusing to delete {d}: not a path this run owns"));
            red = true;
            continue;
        }
        if !mounts_under(d).is_empty() {
            say(&format!("mount check not empty for {d}; not deleting"));
            red = true;
            continue;
        }
        let o = cmd("rm", &["-rf", "-x", d]);
        if !o.ok() {
            say(&format!("rm {d}: {}", o.both().trim()));
            red = true;
        }
    }
    for u in st.all("user") {
        let o = cmd("dscl", &[".", "-delete", &format!("/Users/{u}")]);
        if !o.ok() && !o.both().contains("eDSRecordNotFound") {
            say(&format!("dscl delete user {u}: {}", o.both().trim()));
            red = true;
        }
    }
    for g in st.all("group") {
        let o = cmd("dscl", &[".", "-delete", &format!("/Groups/{g}")]);
        if !o.ok() && !o.both().contains("eDSRecordNotFound") {
            say(&format!("dscl delete group {g}: {}", o.both().trim()));
            red = true;
        }
    }
    for img in st.all("image") {
        if img.contains(&ctx.run) {
            let _ = fs::remove_file(&img);
        }
    }
    if let Some(b) = st.last("exports_backup") {
        let _ = fs::remove_file(b);
    }
    say(if red { "finished with problems" } else { "clean" });
    i32::from(red)
}

/// created: remove the file if it holds only our lines, else only our lines. appended: restore
/// the backup if nothing else changed the file, else remove only our lines.
fn restore_exports(st: &State, mode: &str) -> Result<(), String> {
    let ours = st.all("exports_line");
    let cur = match fs::read_to_string("/etc/exports") {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    let kept: Vec<&str> = cur.lines().filter(|l| !ours.iter().any(|o| o == l)).collect();
    let backup: Option<PathBuf> = st.last("exports_backup").map(PathBuf::from);
    if mode == "appended" {
        if let Some(b) = backup.as_ref().filter(|b| b.exists()) {
            let orig = fs::read_to_string(b).map_err(|e| e.to_string())?;
            let orig_lines: Vec<&str> = orig.lines().collect();
            if kept == orig_lines {
                return fs::write("/etc/exports", orig).map_err(|e| e.to_string());
            }
        }
    }
    if kept.iter().all(|l| l.trim().is_empty()) && mode == "created" {
        return fs::remove_file("/etc/exports").map_err(|e| e.to_string());
    }
    fs::write(
        "/etc/exports",
        kept.join("\n") + if kept.is_empty() { "" } else { "\n" },
    )
    .map_err(|e| e.to_string())
}

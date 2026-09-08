use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::*;

fn seed(dir: &Path, names: impl IntoIterator<Item = impl AsRef<Path>>) {
    for name in names {
        std::fs::write(dir.join(name), b"").expect("seed an entry");
    }
}

fn found(dir: &Path) -> BTreeSet<OsString> {
    names(dir).expect("snapshot").into_iter().collect()
}

#[skuld::test]
fn every_name_comes_back_and_the_dot_entries_do_not() {
    let dir = tempfile::tempdir().expect("temp dir");
    seed(dir.path(), ["a.service", ".hidden", "b.service.d"]);

    let names = found(dir.path());

    assert_eq!(
        names,
        ["a.service", ".hidden", "b.service.d"]
            .into_iter()
            .map(OsString::from)
            .collect::<BTreeSet<_>>()
    );
}

#[skuld::test]
fn an_empty_directory_has_no_names() {
    let dir = tempfile::tempdir().expect("temp dir");
    assert!(found(dir.path()).is_empty());
}

/// The buffer has to hold the *whole* directory for one `getdents64` to be an instant rather than a
/// prefix of one, so a directory that overflows the first attempt is re-read into a larger buffer
/// rather than stitched together from two calls.
#[skuld::test]
fn a_directory_that_overflows_the_first_buffer_still_comes_back_whole() {
    let dir = tempfile::tempdir().expect("temp dir");
    let seeded: BTreeSet<OsString> = (0..500)
        .map(|n| OsString::from(format!("entry-{n:04}-with-a-name-long-enough-to-fill-a-record.service")))
        .collect();
    seed(dir.path(), &seeded);

    // Smaller than one record, so `names_from`'s own floor and then its growth are both exercised.
    let names: BTreeSet<OsString> = names_from(dir.path(), 1).expect("snapshot").into_iter().collect();

    assert_eq!(names, seeded);
}

/// Unit file names are bytes, not text: `manager::collect_units` renders them lossily for its
/// messages but reads through the path itself, so nothing here may decode them.
#[skuld::test]
fn a_name_that_is_not_utf8_survives() {
    use std::os::unix::ffi::OsStrExt as _;

    let dir = tempfile::tempdir().expect("temp dir");
    let name = OsString::from_vec(b"not-\xff-utf8.service".to_vec());
    seed(dir.path(), [&name]);

    let names = found(dir.path());

    assert_eq!(names.len(), 1, "{names:?}");
    assert_eq!(
        names.iter().next().expect("the one name").as_os_str().as_bytes(),
        b"not-\xff-utf8.service"
    );
}

/// A directory that is not there is an established absence and stays the caller's to read — the
/// answer `scan_unit_dir` turns into an empty scan with nothing outstanding.
#[skuld::test]
fn an_absent_directory_is_not_found() {
    let dir = tempfile::tempdir().expect("temp dir");

    let e = names(&dir.path().join("nope")).expect_err("no such directory");

    assert_eq!(e.kind(), std::io::ErrorKind::NotFound, "{e:?}");
}

/// The property the whole module exists for: a name being renamed *within* the directory is in the
/// snapshot under one of its two names, always — never neither.
///
/// Eight pairs rather than one because whether a `read_dir` cursor can lose a given pair depends on
/// how the two names hash against each other, which is a per-name coin flip; one pair leaves a
/// regression a better-than-even chance of passing. The `caught_mid_rename` count is what keeps the
/// assertion from being vacuous: it is proof the reader and the renamers actually interleaved, on a
/// host where the filesystem or the scheduler might otherwise never let them.
#[skuld::test]
fn a_name_being_renamed_within_the_directory_is_never_missed() {
    let dir = tempfile::tempdir().expect("temp dir");
    // Enough entries that a cursor-based read would need more than one `getdents64` for them.
    seed(dir.path(), (0..1500).map(|n| format!("filler-{n:04}.padding")));

    let pairs: Vec<(OsString, OsString)> = (0..8)
        .map(|n| {
            (
                OsString::from(format!("pair-{n}.service")),
                OsString::from(format!(".pair-{n}.service.goetia-quarantine.0-0")),
            )
        })
        .collect();
    seed(dir.path(), pairs.iter().map(|(here, _)| here));

    let stop = Arc::new(AtomicBool::new(false));
    let renames = Arc::new(AtomicU64::new(0));
    let renamers: Vec<_> = pairs
        .iter()
        .map(|(here, there)| {
            let (here, there) = (dir.path().join(here), dir.path().join(there));
            let (stop, renames) = (Arc::clone(&stop), Arc::clone(&renames));
            std::thread::spawn(move || {
                // A flag, never a deadline: the reader below stops these when its own workload is
                // done, and joins them before the directory is removed.
                while !stop.load(Ordering::Relaxed) {
                    std::fs::rename(&here, &there).expect("rename aside");
                    std::fs::rename(&there, &here).expect("rename back");
                    renames.fetch_add(2, Ordering::Relaxed);
                }
            })
        })
        .collect();

    let mut caught_mid_rename = 0;
    let mut missed = Vec::new();
    for _ in 0..2000 {
        let names = found(dir.path());
        for (here, there) in &pairs {
            match (names.contains(here), names.contains(there)) {
                (false, false) => missed.push(here.clone()),
                (_, true) => caught_mid_rename += 1,
                _ => {}
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    for renamer in renamers {
        renamer.join().expect("a renamer thread");
    }

    assert!(
        missed.is_empty(),
        "a name renamed within the directory was in the snapshot under neither of its two names, \
         which is how a listing comes to omit an installed daemon: {missed:?}"
    );
    assert!(
        renames.load(Ordering::Relaxed) > 0,
        "the renamers have to have moved something for this to prove anything"
    );
    assert!(
        caught_mid_rename > 0,
        "no snapshot ever landed inside a rename, so this run proves nothing about one"
    );
}

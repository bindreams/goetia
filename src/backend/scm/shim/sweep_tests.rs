//! `unreferenced` on every host, over stated paths. The Windows-only tests read temp directories
//! the test made and nothing else.

use std::path::PathBuf;

use super::*;

const HEX_A: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
const HEX_B: &str = "cb7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

fn current(dir: &str, hex: &str) -> PathBuf {
    PathBuf::from(format!("{dir}/goetia-shim-{hex}.exe"))
}

fn aside(dir: &str, hex: &str) -> PathBuf {
    PathBuf::from(format!("{dir}/goetia-shim-{hex}.k3j9.stale.exe"))
}

#[skuld::test]
fn unreferenced_keeps_referenced_case_insensitively() {
    let files = [current("/p/Goetia", HEX_A), current("/p/Goetia", HEX_B)];
    let referenced = [PathBuf::from(format!(
        "/P/GOETIA/GOETIA-SHIM-{}.EXE",
        HEX_A.to_uppercase()
    ))];

    assert_eq!(unreferenced(&files, &referenced), vec![current("/p/Goetia", HEX_B)]);
}

#[skuld::test]
fn unreferenced_matches_stale_aside_files() {
    let files = [aside("/p", HEX_A), current("/p", HEX_A)];

    assert_eq!(
        unreferenced(&files, &[]),
        vec![aside("/p", HEX_A), current("/p", HEX_A)]
    );
    // A renamed-aside file a running process has as its image is as referenced as any other.
    assert_eq!(unreferenced(&files, &[aside("/p", HEX_A)]), vec![current("/p", HEX_A)]);
}

#[skuld::test]
fn unreferenced_ignores_other_files() {
    let files = [
        PathBuf::from("/p/goetia-shim.exe"),
        PathBuf::from("/p/notes.txt"),
        PathBuf::from(format!("/p/goetia-shim-{HEX_A}.exe.bak")),
        current("/p", HEX_A),
    ];

    assert_eq!(unreferenced(&files, &[]), vec![current("/p", HEX_A)]);
}

/// A process that could not be opened is known by its file name alone, which references every file of
/// that name, whatever its folder.
#[skuld::test]
fn unreferenced_keeps_every_file_of_a_referenced_bare_name() {
    let files = [current("/a", HEX_A), current("/b", HEX_A), current("/a", HEX_B)];
    let referenced = [PathBuf::from(format!("GOETIA-SHIM-{HEX_A}.exe"))];

    assert_eq!(unreferenced(&files, &referenced), vec![current("/a", HEX_B)]);
}

/// What SCM starts for `"C:\Program Files\Goetia\a.exe" -k x` is the quoted part alone.
#[skuld::test]
fn a_quoted_image_path_names_its_quoted_part() {
    assert_eq!(
        host_candidates(r#""C:\Program Files\Goetia\a.exe" -k x"#),
        [r"C:\Program Files\Goetia\a.exe"]
    );
    assert_eq!(
        host_candidates(r#""C:\a b\c.exe"#),
        [r"C:\a b\c.exe"],
        "an unterminated quote runs to the end"
    );
}

/// SCM tries each space as the end of the path, so an unquoted path with spaces may be any of these.
#[skuld::test]
fn an_unquoted_image_path_names_every_prefix_that_ends_before_a_space_and_the_whole() {
    assert_eq!(
        host_candidates(r"C:\Program Files\Goetia\a.exe -k x"),
        [
            r"C:\Program",
            r"C:\Program.exe",
            r"C:\Program Files\Goetia\a.exe",
            r"C:\Program Files\Goetia\a.exe -k",
            r"C:\Program Files\Goetia\a.exe -k x",
        ]
    );
}

#[skuld::test]
fn an_unquoted_image_path_without_spaces_names_itself() {
    assert_eq!(host_candidates(r"C:\a\b.exe"), [r"C:\a\b.exe"]);
    assert_eq!(host_candidates("  "), Vec::<String>::new());
    assert_eq!(host_candidates(""), Vec::<String>::new());
}

/// `CreateProcess` appends `.exe` to an executable path with no extension.
#[skuld::test]
fn a_candidate_with_no_extension_is_also_named_with_exe() {
    assert_eq!(
        host_candidates(r"C:\a\goetia-shim-x"),
        [r"C:\a\goetia-shim-x", r"C:\a\goetia-shim-x.exe"]
    );
    assert_eq!(host_candidates(r"C:\a\b.exe"), [r"C:\a\b.exe"]);
}

fn shim_name(hex: &str) -> String {
    format!("goetia-shim-{hex}.exe")
}

/// A registration with a `%VAR%` in it is also known by its file name, since the expansion here may
/// not be the one SCM makes, and may have failed.
#[skuld::test]
fn an_image_path_with_a_variable_also_references_by_file_name() {
    let raw = format!(r"%ProgramFiles%\Goetia\{}", shim_name(HEX_A));
    let expanded = format!(r"C:\Program Files\Goetia\{}", shim_name(HEX_A));

    let references = image_references(&raw, &expanded);

    assert!(references.contains(&expanded), "{references:?}");
    assert!(references.contains(&shim_name(HEX_A)), "by name: {references:?}");
}

#[skuld::test]
fn an_expansion_that_changed_nothing_still_references_by_file_name() {
    let raw = format!(r"%Unset%\{}", shim_name(HEX_A));

    assert!(image_references(&raw, &raw).contains(&shim_name(HEX_A)));
}

#[skuld::test]
fn an_extensionless_image_path_with_a_variable_references_the_name_with_exe() {
    let raw = format!(r"%ProgramFiles%\Goetia\goetia-shim-{HEX_A}");

    assert!(image_references(&raw, &raw).contains(&shim_name(HEX_A)));
}

#[skuld::test]
fn an_image_path_without_a_variable_is_not_a_name() {
    let path = format!(r"C:\Goetia\{}", shim_name(HEX_A));

    assert_eq!(image_references(&path, &path), [path]);
}

#[skuld::test]
fn only_shim_file_names_are_referenced_by_name() {
    assert_eq!(
        image_references(
            r"%SystemRoot%\system32\svchost.exe -k x",
            r"C:\Windows\system32\svchost.exe -k x"
        )
        .iter()
        .filter(|reference| !reference.contains('\\'))
        .count(),
        0
    );
}

#[cfg(windows)]
mod windows {
    use std::ffi::OsString;
    use std::fs::{self, File, OpenOptions};
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::path::Path;

    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE, GetShortPathNameW};

    use super::system::{expand_environment, remove_if_empty_default};
    use super::*;
    use crate::backend::scm::shim::acl::acl_tests::windows::junction;
    use crate::error::Error;

    const HEX_C: &str = "cc7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const HEX_F: &str = "ff7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn shim_in(dir: &Path, hex: &str) -> PathBuf {
        dir.join(format!("goetia-shim-{hex}.exe"))
    }

    /// `path` by its 8.3 names where the volume keeps them, and unchanged where it does not.
    fn short(path: &Path) -> PathBuf {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let mut buf = vec![0u16; 1024];
        // SAFETY: `wide` is NUL-terminated and `buf` is `buf.len()` writable units.
        let len = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        assert!(
            0 < len && len < buf.len(),
            "GetShortPathNameW: {}",
            std::io::Error::last_os_error()
        );
        PathBuf::from(OsString::from_wide(&buf[..len]))
    }

    #[skuld::test]
    fn normalise_equates_spellings() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let file = shim_in(dir.path(), HEX_A);
        fs::write(&file, b"x").unwrap();
        let link = dir.path().join("link");
        junction(&link, dir.path());
        let name = file.file_name().unwrap();
        let shown = file.to_string_lossy().into_owned();
        let final_path = fs::canonicalize(&file).unwrap();

        let spellings = [
            file.clone(),
            short(&file),
            final_path.clone(),
            dir.path().join("sub").join("..").join(name),
            PathBuf::from(shown.replace('\\', r"\\")),
            PathBuf::from(shown.replace('\\', "/")),
            link.join(name),
            PathBuf::from(shown.to_uppercase()),
        ];

        let wanted = normalise(&file);
        assert!(!wanted.starts_with(r"\\?\"), "{}", wanted.display());
        assert_eq!(wanted.file_name(), Some(name));
        for spelling in &spellings {
            assert_eq!(normalise(spelling), wanted, "{}", spelling.display());
        }
    }

    #[skuld::test]
    fn normalise_of_a_missing_file_is_lexical() {
        assert_eq!(
            normalise(Path::new(r"C:\goetia-sweep-test-missing\a\..\\b\.\c.exe")),
            PathBuf::from(r"C:\goetia-sweep-test-missing\b\c.exe")
        );
    }

    #[skuld::test]
    fn sweep_reports_every_failure() {
        let dir = tempfile::tempdir().unwrap();
        // Held open without delete sharing: deleting each fails with a sharing violation.
        let held: Vec<(PathBuf, File)> = [HEX_A, HEX_B, HEX_C]
            .iter()
            .map(|hex| {
                let path = shim_in(dir.path(), hex);
                fs::write(&path, b"x").unwrap();
                let file = OpenOptions::new()
                    .read(true)
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                    .open(&path)
                    .unwrap();
                (path, file)
            })
            .collect();
        // Sorts after the held ones: deleted only if the sweep goes on past their failures.
        let free = shim_in(dir.path(), HEX_F);
        fs::write(&free, b"x").unwrap();
        let other = dir.path().join("notes.txt");
        fs::write(&other, b"x").unwrap();

        let error = sweep_folder(dir.path(), &[]).expect_err("three files cannot be deleted");

        let report = error.report().to_string();
        let mut at = 0;
        for (path, _) in &held {
            let found = report[at..].find(&path.display().to_string());
            assert!(found.is_some(), "{} in order in: {report}", path.display());
            at += found.unwrap();
        }
        assert!(!free.exists(), "the sweep stopped at the first failure");
        assert!(other.exists(), "a file that is not a shim file is never touched");
        assert!(held.iter().all(|(path, _)| path.exists()));
    }

    #[skuld::test]
    fn sweep_folder_keeps_an_explicit_referenced_file() {
        let dir = tempfile::tempdir().unwrap();
        let (kept, swept) = (shim_in(dir.path(), HEX_A), shim_in(dir.path(), HEX_B));
        fs::write(&kept, b"x").unwrap();
        fs::write(&swept, b"x").unwrap();

        // The reference is spelled another way than the folder: both are normalised.
        sweep_folder(dir.path(), &[normalise(&short(&kept))]).unwrap();

        assert!(kept.exists());
        assert!(!swept.exists());
    }

    /// What a repair sets a folder at the shim's name aside as.
    #[skuld::test]
    fn sweep_removes_a_folder_a_repair_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let aside = dir.path().join(format!("goetia-shim-{HEX_A}.k3j9.stale.exe"));
        fs::create_dir(&aside).unwrap();
        fs::write(aside.join("inside"), b"x").unwrap();

        sweep_folder(dir.path(), &[]).unwrap();

        assert!(!aside.exists());
    }

    /// A folder that cannot be listed is said so once, not by the sweep of its files and again by
    /// the check whether it is empty.
    #[skuld::test]
    fn a_folder_that_cannot_be_listed_is_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_folder = dir.path().join("file.txt");
        fs::write(&not_a_folder, b"x").unwrap();

        let error = sweep_folder(&not_a_folder, &[]).expect_err("a file cannot be listed");

        let report = error.report().to_string();
        assert_eq!(report.matches("file.txt").count(), 1, "{report}");
    }

    #[skuld::test]
    fn sweep_of_a_folder_that_is_not_there_is_nothing_to_do() {
        let dir = tempfile::tempdir().unwrap();

        sweep_folder(&dir.path().join("missing"), &[]).unwrap();
    }

    #[skuld::test]
    fn an_empty_folder_is_removed_only_when_it_is_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Goetia");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("notes.txt"), b"x").unwrap();

        remove_if_empty_default(&folder, || Ok(true)).unwrap();
        assert!(folder.is_dir(), "a folder that holds anything stays");

        fs::remove_file(folder.join("notes.txt")).unwrap();
        remove_if_empty_default(&folder, || Ok(false)).unwrap();
        assert!(folder.is_dir(), "a folder that is not the default stays");

        remove_if_empty_default(&folder, || Ok(true)).unwrap();
        assert!(!folder.exists());
        remove_if_empty_default(&folder, || unreachable!("a folder that is gone is not asked about")).unwrap();
    }

    /// Where the default folder is matters only for a folder that is empty: a failure to find it is
    /// not reported for one that is not, and does not fail the sweep of the files.
    #[skuld::test]
    fn a_default_folder_that_cannot_be_located_is_reported_only_when_it_matters() {
        let dir = tempfile::tempdir().unwrap();
        let unlocated = || Err(Error::os("locate %ProgramFiles%", std::io::Error::other("no")));
        fs::write(dir.path().join("notes.txt"), b"x").unwrap();

        remove_if_empty_default(dir.path(), unlocated).unwrap();

        fs::remove_file(dir.path().join("notes.txt")).unwrap();
        let error = remove_if_empty_default(dir.path(), unlocated).expect_err("an empty folder needs the answer");
        assert!(
            error.report().to_string().contains("locate %ProgramFiles%"),
            "{}",
            error.report()
        );
        assert!(dir.path().is_dir());
    }

    #[skuld::test]
    fn environment_variables_in_an_image_path_are_expanded() {
        let root = std::env::var("SystemRoot").expect("SystemRoot");

        assert_eq!(expand_environment(r"%SystemRoot%\x.exe"), format!(r"{root}\x.exe"));
        assert_eq!(
            expand_environment(r"%goetia_no_such_variable%\x.exe"),
            r"%goetia_no_such_variable%\x.exe"
        );
        assert_eq!(expand_environment(r"C:\a\b.exe"), r"C:\a\b.exe");
    }
}

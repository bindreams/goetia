//! `shim_file` on every host. The Windows-only tests stop short of `provide`, which
//! `tests/shim_integration/provision.rs` covers; the ones that set an owner (Administrators) need
//! an elevated token.

use super::*;

const HEX: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

#[skuld::test]
fn shim_file_matches_only_shim_names() {
    let current = format!("goetia-shim-{HEX}.exe");
    let aside = format!("goetia-shim-{HEX}.a1B2c3.stale.exe");

    assert!(matches!(shim_file(&current), Some(ShimFile::Current)));
    assert!(matches!(shim_file(&aside), Some(ShimFile::StaleAside)));
    assert!(matches!(shim_file(&current.to_uppercase()), Some(ShimFile::Current)));
    assert!(matches!(shim_file(&aside.to_uppercase()), Some(ShimFile::StaleAside)));
}

#[skuld::test]
fn shim_file_refuses_what_is_not_a_shim_name() {
    for name in [
        "goetia-shim-notes.exe",
        "goetia-shim.exe",
        "goetia.exe",
        &format!("goetia-shim-{}.exe", &HEX[..63]),
        &format!("goetia-shim-{HEX}0.exe"),
        &format!("goetia-shim-{}g.exe", &HEX[..63]),
        &format!("goetia-shim-{HEX}.exe.bak"),
        &format!("goetia-shim-{HEX}.stale.exe"),
        &format!("goetia-shim-{HEX}..stale.exe"),
        &format!("goetia-shim-{HEX}.x.stale.dll"),
        &format!("other-goetia-shim-{HEX}.exe"),
        &format!("goetia-shim-{}\u{e9}.exe", &HEX[..62]),
        "",
    ] {
        assert!(shim_file(name).is_none(), "{name}");
    }
}

#[cfg(windows)]
mod windows {
    use std::io::{self, Cursor, Read, Write as _};
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS};

    use super::stock::*;
    use super::*;
    use crate::backend::scm::shim::acl::{CheckPermissions, Descriptor, FolderCheck};
    use crate::backend::test_elevation::{ELEVATED, elevated};
    use crate::error::{Error, Result};
    use crate::shim::{HostCommand, ShimProvider};

    fn disabled() -> FolderCheck {
        FolderCheck::new(CheckPermissions::Disable)
    }

    #[skuld::test]
    fn file_name_is_the_hash_of_the_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let provider = EmbeddedStockShim::new(Some(b"abc"), Some(dir.path().to_path_buf()));

        assert_eq!(
            provider.preview().unwrap(),
            HostCommand {
                exe: dir.path().join(format!("goetia-shim-{HEX}.exe")),
                args: vec![],
            }
        );
    }

    /// The bytes are `'static` and the digest is of them alone, so the first use keeps the image, digest
    /// included, for every later one.
    #[skuld::test]
    fn the_image_of_the_embedded_bytes_is_made_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let provider = EmbeddedStockShim::new(Some(b"abc"), Some(dir.path().to_path_buf()));
        assert!(provider.image.get().is_none(), "nothing is hashed before it is needed");

        provider.preview().unwrap();

        assert_eq!(
            provider.image.get().map(Image::digest),
            Some(Image::new(b"abc").digest())
        );
    }

    #[skuld::test]
    fn another_content_is_another_name() {
        let dir = tempfile::tempdir().unwrap();
        let named = |bytes: &'static [u8]| {
            EmbeddedStockShim::new(Some(bytes), Some(dir.path().to_path_buf()))
                .preview()
                .unwrap()
                .exe
        };

        assert_ne!(named(b"abc"), named(b"abd"));
        assert_eq!(named(b"abc"), named(b"abc"));
    }

    #[skuld::test]
    fn the_stale_names_a_fresh_file_is_written_under_are_the_sweeps_to_match() {
        let image = Image::new(b"abc");

        assert_eq!(image.hex(), HEX);
        let name = image.stale_prefix() + "x1y2z3" + STALE_SUFFIX;
        assert!(matches!(shim_file(&name), Some(ShimFile::StaleAside)), "{name}");
    }

    #[skuld::test]
    fn embed_shim_is_none_without_the_feature() {
        assert!(crate::embed_shim!().is_none());
    }

    #[skuld::test]
    fn none_bytes_report_not_embedded() {
        let dir = tempfile::tempdir().unwrap();
        let provider = EmbeddedStockShim::new(None, Some(dir.path().to_path_buf()));

        assert!(matches!(provider.preview(), Err(Error::ShimNotEmbedded)));
        assert!(matches!(provider.provide(&disabled()), Err(Error::ShimNotEmbedded)));
        assert!(
            std::fs::read_dir(dir.path()).unwrap().next().is_none(),
            "nothing may be written"
        );
    }

    #[skuld::test]
    fn provide_refuses_a_relative_dir() {
        let provider = EmbeddedStockShim::new(Some(b"abc"), Some(PathBuf::from(r"shims\here")));

        for result in [provider.preview(), provider.provide(&disabled())] {
            match result {
                Err(Error::ShimPathRelative { path }) => assert_eq!(path, PathBuf::from(r"shims\here")),
                other => panic!("a relative dir must be refused, got {other:?}"),
            }
        }
        assert!(
            !PathBuf::from(r"shims\here").exists(),
            "nothing may be created under the working directory"
        );
    }

    /// `folder_in(None)`, which `SiblingStockShim` uses, is the default folder, and an absolute
    /// folder is kept as given.
    #[skuld::test]
    fn the_stock_folder_is_the_given_one_or_the_default() {
        assert_eq!(
            folder_in(Some(Path::new(r"C:\shims"))).unwrap(),
            PathBuf::from(r"C:\shims")
        );
        assert_eq!(folder_in(None).unwrap(), default_dir().unwrap());
    }

    #[skuld::test]
    fn a_relative_dir_is_no_folder_and_sweeps_nothing() {
        let provider = EmbeddedStockShim::new(Some(b"abc"), Some(PathBuf::from(r"shims\here")));

        match provider.folder() {
            Err(Error::ShimPathRelative { path }) => assert_eq!(path, PathBuf::from(r"shims\here")),
            other => panic!("a relative dir has no folder, got {other:?}"),
        }
        let failure = provider
            .sweep(&[], &disabled())
            .expect_err("a relative dir cannot be swept");
        assert!(
            matches!(&*failure.error, Error::ShimPathRelative { path } if path == Path::new(r"shims\here")),
            "{:?}",
            failure.error
        );
        assert!(failure.not_deleted.is_empty());
        assert!(!Path::new(r"shims\here").exists());
    }

    #[skuld::test]
    fn an_absolute_dir_is_the_folder() {
        let provider = EmbeddedStockShim::new(None, Some(PathBuf::from(r"C:\shims")));

        assert_eq!(provider.folder().unwrap(), Some(PathBuf::from(r"C:\shims")));
    }

    /// The default folder does not depend on the sibling.
    #[cfg(feature = "cli")]
    #[skuld::test]
    fn the_siblings_folder_is_the_default_one_without_the_sibling() {
        let beside = std::env::current_exe().unwrap().with_file_name("goetia-shim.exe");
        assert!(
            !beside.exists(),
            "this test needs a test binary with no stock shim beside it: {}",
            beside.display()
        );

        assert_eq!(SiblingStockShim.folder().unwrap(), Some(default_dir().unwrap()));
    }

    #[cfg(feature = "cli")]
    #[skuld::test]
    fn sibling_missing_names_the_path() {
        let beside = std::env::current_exe().unwrap().with_file_name("goetia-shim.exe");
        assert!(
            !beside.exists(),
            "this test needs a test binary with no stock shim beside it: {}",
            beside.display()
        );

        for result in [SiblingStockShim.preview(), SiblingStockShim.provide(&disabled())] {
            match result {
                Err(Error::SiblingShimMissing { path }) => assert_eq!(path, beside),
                other => panic!("a missing sibling must name its path, got {other:?}"),
            }
        }
    }

    #[skuld::test]
    fn locate_error_names_program_files() {
        // The facility of a Win32 error, and `E_FAIL`.
        for hresult in [0x8007_0057_u32 as i32, 0x8000_4005_u32 as i32] {
            match locate_error(hresult) {
                Error::Os { what, path, source } => {
                    assert_eq!(what, "locate %ProgramFiles%");
                    assert_eq!(path, None);
                    assert!(!source.to_string().is_empty());
                }
                other => panic!("expected an OS error, got {other:?}"),
            }
        }
    }

    #[skuld::test]
    fn the_default_folder_is_goetia_under_program_files() {
        let dir = default_dir().unwrap();

        assert!(dir.is_absolute(), "{}", dir.display());
        assert_eq!(dir.file_name().unwrap(), "Goetia");
        assert_eq!(
            dir.parent().unwrap().to_str().map(str::to_lowercase),
            std::env::var("ProgramW6432")
                .or_else(|_| std::env::var("ProgramFiles"))
                .ok()
                .map(|p| p.to_lowercase()),
        );
    }

    /// A reader that hands out at most `step` bytes per `read`.
    struct Trickle<R> {
        inner: R,
        step: usize,
    }

    impl<R: Read> Read for Trickle<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = buf.len().min(self.step);
            self.inner.read(&mut buf[..n])
        }
    }

    #[skuld::test]
    fn a_streamed_hash_is_the_hash_of_the_bytes_however_they_arrive() {
        let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let whole = Image::new(&bytes).digest();

        for step in [1, 7, 65_536, 1 << 20] {
            let streamed = hash_reader(Trickle {
                inner: Cursor::new(&bytes),
                step,
            })
            .unwrap();
            assert_eq!(streamed, whole, "step {step}");
        }
        assert_eq!(hash_reader(Cursor::new(Vec::new())).unwrap(), Image::new(b"").digest());
    }

    #[skuld::test]
    fn the_folders_to_create_are_those_below_the_first_that_exists_outermost_first() {
        let root = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            root.path().join("a"),
            root.path().join("a").join("b"),
            root.path().join("a").join("b").join("c"),
        );

        assert_eq!(missing_components(&c).unwrap(), [a.clone(), b.clone(), c.clone()]);
        std::fs::create_dir(&a).unwrap();
        assert_eq!(missing_components(&c).unwrap(), [b, c]);
        assert!(missing_components(&a).unwrap().is_empty());
    }

    /// A folder found missing that exists when it is created was made by someone else in between,
    /// which goetia cannot tell from a hostile one: it is refused, not adopted.
    #[skuld::test]
    fn a_folder_that_appeared_after_it_was_found_missing_is_refused() {
        let path = Path::new(r"C:\Goetia");

        let refused = created(path, io::Error::from_raw_os_error(ERROR_ALREADY_EXISTS as i32));
        let failed = created(path, io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32));

        let Error::Step { what, source } = &refused else {
            panic!("expected a refusal, got {refused:?}");
        };
        assert!(what.contains(r"C:\Goetia") && what.contains("appeared"), "{what}");
        assert!(matches!(**source, Error::AlreadyExists { .. }), "{source:?}");
        assert!(matches!(failed, Error::Os { .. }), "{failed:?}");
    }

    /// The race itself needs a folder to appear between the two steps, which no test can arrange
    /// without a seam: the folder that "appeared" is one already in the list.
    #[skuld::test(requires = [elevated], labels = [ELEVATED])]
    fn creating_stops_at_a_folder_that_exists_and_leaves_it_alone() {
        let root = tempfile::tempdir().unwrap();
        let (made, taken, after) = (
            root.path().join("made"),
            root.path().join("taken"),
            root.path().join("after"),
        );
        std::fs::create_dir(&taken).unwrap();
        std::fs::write(taken.join("theirs"), b"x").unwrap();

        let refused = create_each(&[made.clone(), taken.clone(), after.clone()]).unwrap_err();

        assert!(matches!(refused, Error::Step { .. }), "{refused:?}");
        assert!(made.is_dir(), "what came before is goetia's own");
        assert!(!after.exists(), "nothing is created past the refusal");
        assert!(taken.join("theirs").is_file(), "the folder that was there is untouched");
    }

    /// What `inspect` says of `path`, as the provider asks it, for the image of `b"abc"`.
    ///
    /// A file a test creates plainly has an owner and DACL that are never the stock ones, which
    /// `inspect` judges after it has opened the file. The cases that end before that (not a file, an
    /// open that is refused) are `Bad` for their own cause: without their branch `inspect` is an
    /// error. The cases after it are made from [`stock_file`] and told apart from the plain one.
    fn inspected(path: &Path) -> Result<Existing> {
        let wanted = Descriptor::for_file().unwrap().wanted().unwrap();
        inspect(path, &Image::new(b"abc"), &wanted)
    }

    /// A file at `path` with the stock owner and permissions and `bytes`, which needs an elevated
    /// token to name Administrators as its owner.
    fn stock_file(path: &Path, bytes: &[u8]) {
        let mut file = Descriptor::for_file().unwrap().create_file(path).unwrap();
        file.write_all(bytes).unwrap();
    }

    #[skuld::test(requires = [elevated], labels = [ELEVATED])]
    fn a_file_with_the_stock_owner_permissions_and_bytes_is_good() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shim.exe");
        stock_file(&path, b"abc");

        assert!(matches!(inspected(&path), Ok(Existing::Good)));
    }

    /// The file is stock but for its bytes, so only the hash can make it `Bad`.
    #[skuld::test(requires = [elevated], labels = [ELEVATED])]
    fn a_stock_file_with_other_bytes_is_bad() {
        let dir = tempfile::tempdir().unwrap();
        let (same, other) = (dir.path().join("same.exe"), dir.path().join("other.exe"));
        stock_file(&same, b"abc");
        stock_file(&other, b"abd");

        assert!(matches!(inspected(&same), Ok(Existing::Good)));
        assert!(matches!(inspected(&other), Ok(Existing::Bad)));
    }

    /// The bytes are right, so only the owner or the permissions can make it `Bad`: the same bytes
    /// in a stock file are `Good`.
    #[skuld::test(requires = [elevated], labels = [ELEVATED])]
    fn a_file_with_the_right_bytes_and_other_permissions_is_bad() {
        let dir = tempfile::tempdir().unwrap();
        let (stock, plain) = (dir.path().join("stock.exe"), dir.path().join("plain.exe"));
        stock_file(&stock, b"abc");
        std::fs::write(&plain, b"abc").unwrap();

        assert!(matches!(inspected(&stock), Ok(Existing::Good)));
        assert!(matches!(inspected(&plain), Ok(Existing::Bad)));
    }

    #[skuld::test]
    fn nothing_at_the_name_is_absent() {
        let dir = tempfile::tempdir().unwrap();

        assert!(matches!(inspected(&dir.path().join("none.exe")), Ok(Existing::Absent)));
    }

    #[skuld::test]
    fn a_directory_at_the_name_is_bad() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shim.exe");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("inside"), b"x").unwrap();

        assert!(matches!(inspected(&path), Ok(Existing::Bad)));
    }

    #[skuld::test]
    fn a_file_that_denies_reading_is_bad_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shim.exe");
        std::fs::write(&path, b"abc").unwrap();
        // Everyone is denied reading the data, the owner included: `File::open` is refused.
        let denied = std::process::Command::new("icacls")
            .arg(&path)
            .args(["/deny", "*S-1-1-0:(RD)"])
            .output()
            .unwrap();
        assert!(denied.status.success(), "{}", String::from_utf8_lossy(&denied.stdout));
        assert!(std::fs::File::open(&path).is_err(), "the denial must refuse an open");

        assert!(matches!(inspected(&path), Ok(Existing::Bad)));
    }

    #[skuld::test]
    fn a_file_held_without_sharing_is_bad_not_an_error() {
        use std::os::windows::fs::OpenOptionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shim.exe");
        std::fs::write(&path, b"abc").unwrap();
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();

        assert!(matches!(inspected(&path), Ok(Existing::Bad)));
    }
}

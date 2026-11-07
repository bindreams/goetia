//! `shim_file` on every host. The Windows-only tests stop short of writing: that sets an owner
//! (Administrators), which needs an elevated token, so `tests/shim_integration/provision.rs` covers
//! `provide`.

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
    use std::io::{self, Cursor, Read};
    use std::path::PathBuf;

    use super::stock::*;
    use super::*;
    use crate::backend::scm::shim::acl::{CheckPermissions, FolderCheck};
    use crate::error::Error;
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

    #[skuld::test]
    fn sibling_missing_names_the_path() {
        let beside = std::env::current_exe().unwrap().with_file_name("goetia-shim.exe");
        assert!(
            !beside.exists(),
            "this test needs a test binary with no stock shim beside it: {}",
            beside.display()
        );

        for result in [
            SiblingStockShim::new().preview(),
            SiblingStockShim::new().provide(&disabled()),
        ] {
            match result {
                Err(Error::SiblingShimMissing { path }) => assert_eq!(path, beside),
                other => panic!("a missing sibling must name its path, got {other:?}"),
            }
        }
        SiblingStockShim::new().sweep(&[]).unwrap();
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
}

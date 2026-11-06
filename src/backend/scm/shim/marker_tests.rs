//! `scan` over buffers and readers, so these run on every host. The Windows-only tests read this
//! binary and temp files this test created.

use std::io::{self, Cursor, Read};

use super::*;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A marker as `scan` must read it: the needle, the version, a NUL.
fn marker_bytes(version: &str) -> Vec<u8> {
    let mut bytes = needle().to_vec();
    bytes.extend_from_slice(version.as_bytes());
    bytes.push(0);
    bytes
}

/// `marker` after `before` filler bytes and before `after` more.
fn host_with(marker: &[u8], before: usize, after: usize) -> Vec<u8> {
    let mut bytes = vec![0xAB; before];
    bytes.extend_from_slice(marker);
    bytes.resize(bytes.len() + after, 0xCD);
    bytes
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
fn scan_names_the_version() {
    let host = host_with(&marker_bytes(VERSION), 100, 100);
    assert_eq!(scan(Cursor::new(host)).unwrap().as_deref(), Some(VERSION));
}

#[skuld::test]
fn scan_names_a_version_of_any_length() {
    for version in ["1", "10.20.30-rc.1+build.5", &"9".repeat(CHUNK * 2)] {
        let host = host_with(&marker_bytes(version), 7, 7);
        assert_eq!(scan(Cursor::new(host)).unwrap().as_deref(), Some(version));
    }
}

#[skuld::test]
fn scan_reports_missing() {
    let bytes: Vec<u8> = (0..CHUNK * 3).map(|i| (i.wrapping_mul(31) >> 3) as u8).collect();
    assert_eq!(scan(Cursor::new(bytes)).unwrap(), None);
    assert_eq!(scan(Cursor::new(Vec::new())).unwrap(), None);
}

#[skuld::test]
fn scan_reports_a_marker_cut_off_before_its_version_ends_as_missing() {
    let mut bytes = marker_bytes(VERSION);
    bytes.pop();
    assert_eq!(scan(Cursor::new(bytes)).unwrap(), None);
}

#[skuld::test]
fn scan_finds_a_marker_across_the_buffer_boundary() {
    // Every placement from "the whole marker ends just before the boundary" to "it starts just
    // after it", which cuts the needle and then the version at every position.
    let marker = marker_bytes(VERSION);
    for start in CHUNK - marker.len() - 1..=CHUNK + 1 {
        let host = host_with(&marker, start, 50);
        assert_eq!(
            scan(Cursor::new(host)).unwrap().as_deref(),
            Some(VERSION),
            "marker at {start}"
        );
    }
}

#[skuld::test]
fn scan_finds_a_marker_whatever_sizes_the_reader_returns() {
    let marker = marker_bytes(VERSION);
    for step in [1, 2, 3, 7, needle().len(), needle().len() + 1, CHUNK - 1] {
        let host = host_with(&marker, CHUNK + 5, 50);
        let reader = Trickle {
            inner: Cursor::new(host),
            step,
        };
        assert_eq!(scan(reader).unwrap().as_deref(), Some(VERSION), "step {step}");
    }
}

#[skuld::test]
fn scan_returns_a_read_error() {
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("disk gone"))
        }
    }
    assert_eq!(scan(Broken).unwrap_err().to_string(), "disk gone");
}

#[cfg(windows)]
mod windows {
    use std::io::Write;
    use std::path::PathBuf;

    use super::*;
    use crate::error::Error;

    /// `version` with every digit replaced by another digit: the same length, never equal.
    fn other_version(version: &str) -> String {
        let other: String = version
            .chars()
            .map(|c| match c.to_digit(10) {
                Some(d) => char::from_digit((d + 1) % 10, 10).unwrap(),
                None => c,
            })
            .collect();
        assert_ne!(other, version, "a version without digits");
        other
    }

    fn temp_file(dir: &tempfile::TempDir, bytes: &[u8]) -> PathBuf {
        let path = dir.path().join("host.exe");
        std::fs::File::create(&path).unwrap().write_all(bytes).unwrap();
        path
    }

    #[skuld::test]
    fn marker_is_what_scan_reads() {
        assert_eq!(scan(&MARKER[..]).unwrap().as_deref(), Some(VERSION));
    }

    #[skuld::test]
    fn verify_names_both_versions() {
        let found = other_version(VERSION);
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, &host_with(&marker_bytes(&found), 10, 10));
        let Err(Error::ShimMarkerVersion {
            path: at,
            found: got,
            expected,
        }) = verify(&path)
        else {
            panic!("expected ShimMarkerVersion");
        };
        assert_eq!((at, got, expected), (path, found, VERSION.to_string()));
    }

    #[skuld::test]
    fn verify_accepts_this_versions_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, &host_with(&marker_bytes(VERSION), 10, 10));
        verify(&path).unwrap();
    }

    #[skuld::test]
    fn verify_names_a_file_without_a_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, b"MZ not a host");
        let Err(Error::ShimMarkerMissing { path: at }) = verify(&path) else {
            panic!("expected ShimMarkerMissing");
        };
        assert_eq!(at, path);
    }

    #[skuld::test]
    fn verify_reports_a_file_it_cannot_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.exe");
        let Err(Error::Os { what, path: at, .. }) = verify(&path) else {
            panic!("expected Os");
        };
        assert_eq!((what.as_str(), at), ("open", Some(path)));
    }

    #[skuld::test]
    fn verify_finds_this_binary() {
        // This binary keeps the marker only because something reads it, as `run` does.
        std::hint::black_box(&MARKER);
        verify(&std::env::current_exe().unwrap()).unwrap();
    }

    #[skuld::test]
    fn the_needle_occurs_once_in_this_binary() {
        std::hint::black_box(&MARKER);
        let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let needle = needle();
        let count = exe.windows(needle.len()).filter(|w| *w == needle).count();
        assert_eq!(count, 1, "the scanner's own copy of the needle is in the binary");
    }
}

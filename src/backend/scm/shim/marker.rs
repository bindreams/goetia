//! The version marker compiled into every service host, and the check that reads it from a file.
//!
//! A host's file contains `needle ++ version ++ NUL` exactly once: `MARKER`, which `run` keeps in
//! the binary. `verify` streams the file through [`scan`] and compares the version with this
//! library's, so a file that is no host, or a host of another version, is refused before it is
//! registered as a service that runs as `LocalSystem`.
//!
//! The needle is the one thing a binary that links goetia must hold only inside `MARKER`: a
//! second copy in the scanner's own code would be found first in any host. So [`needle`] never
//! exists as a literal: the literal has a hole, filled at run time through an opaque read.
//!
//! [`scan`] is pure over any `Read` and builds on every host; `MARKER` and `verify` are
//! Windows-only.
#![cfg_attr(not(windows), allow(dead_code))]

use std::hint::black_box;
use std::io::{self, Read};

use memchr::{memchr, memmem};

#[cfg(windows)]
use std::{fs::File, path::Path};

#[cfg(windows)]
use crate::error::{Error, Result};

/// The needle with its last byte left open.
const TEMPLATE: [u8; 19] = *b"goetia-host-marker\0";
const HOLE: usize = TEMPLATE.len() - 1;
const FILL: u8 = b':';

/// How much of a file `scan` holds at once.
const CHUNK: usize = 256 * 1024;

/// What precedes the version in a host's bytes. Composed on each call, so that the scanner holds
/// no contiguous copy of it.
fn needle() -> [u8; TEMPLATE.len()] {
    let mut needle = TEMPLATE;
    needle[HOLE] = black_box(FILL);
    needle
}

/// The marker of a host built from this library: the needle, this crate's exact version, a NUL.
/// `run` reads it through `black_box`, which is what keeps it in the linked binary.
#[cfg(windows)]
pub(crate) static MARKER: [u8; MARKER_LEN] = marker();

#[cfg(windows)]
const VERSION: &str = env!("CARGO_PKG_VERSION");
#[cfg(windows)]
const MARKER_LEN: usize = TEMPLATE.len() + VERSION.len() + 1;

#[cfg(windows)]
const fn marker() -> [u8; MARKER_LEN] {
    let mut marker = [0u8; MARKER_LEN];
    let mut i = 0;
    while i < TEMPLATE.len() {
        marker[i] = TEMPLATE[i];
        i += 1;
    }
    marker[HOLE] = FILL;
    let version = VERSION.as_bytes();
    let mut j = 0;
    while j < version.len() {
        marker[TEMPLATE.len() + j] = version[j];
        j += 1;
    }
    marker
}

/// The version of the first marker in `file`, or `None` when it holds none (or one cut off before
/// its end). At most the first `keep` bytes of the version are returned, however long it is: the
/// scan still reads to its NUL, so memory stays constant. One pass over the file, through a fixed
/// buffer that carries the last bytes of a read into the next, so a marker across two reads is
/// found whatever they return.
pub(crate) fn scan(mut file: impl Read, keep: usize) -> io::Result<Option<String>> {
    let needle = needle();
    let finder = memmem::Finder::new(&needle);
    let mut buf = vec![0u8; CHUNK];
    let mut held = 0;
    loop {
        let read = read_retrying(&mut file, &mut buf[held..])?;
        if read == 0 {
            return Ok(None);
        }
        let end = held + read;
        if let Some(at) = finder.find(&buf[..end]) {
            return version_after(&buf[at + needle.len()..end], file, keep);
        }
        // A marker not yet complete ends in at most the last `needle.len() - 1` bytes.
        held = (needle.len() - 1).min(end);
        buf.copy_within(end - held..end, 0);
    }
}

/// The first `keep` bytes up to the first NUL, starting with `head` and continuing into `file`.
fn version_after(head: &[u8], mut file: impl Read, keep: usize) -> io::Result<Option<String>> {
    let mut version = Vec::new();
    let mut more = vec![0u8; 4096];
    let mut chunk = head;
    loop {
        let (text, nul) = match memchr(0, chunk) {
            Some(nul) => (&chunk[..nul], true),
            None => (chunk, false),
        };
        version.extend_from_slice(&text[..text.len().min(keep - version.len())]);
        if nul {
            return Ok(Some(String::from_utf8_lossy(&version).into_owned()));
        }
        let read = read_retrying(&mut file, &mut more)?;
        if read == 0 {
            return Ok(None);
        }
        chunk = &more[..read];
    }
}

pub(crate) fn read_retrying(file: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match file.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            other => return other,
        }
    }
}

/// Reads `host` and refuses it unless it carries the marker of exactly this library's version. Only
/// the first `version.len() + 1` bytes of a found version are kept, so a longer one is a mismatch
/// that names its prefix. A file holding more than one marker (a host that embeds another goetia
/// host's bytes) is judged by the first.
#[cfg(windows)]
pub(crate) fn verify(host: &Path) -> Result<()> {
    let file = File::open(host).map_err(|e| Error::os_at("open", host, e))?;
    match scan(file, crate::version().len() + 1).map_err(|e| Error::os_at("read", host, e))? {
        None => Err(Error::ShimMarkerMissing { path: host.into() }),
        Some(found) if found != crate::version() => Err(Error::ShimMarkerVersion {
            path: host.into(),
            found,
            expected: crate::version().to_string(),
        }),
        Some(_) => Ok(()),
    }
}

#[cfg(test)]
#[path = "marker_tests.rs"]
mod marker_tests;

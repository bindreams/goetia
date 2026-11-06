use super::*;
use crate::backend::scm::shim::acl::{CheckPermissions, FolderCheck};

#[skuld::test]
fn no_shim_has_nothing_to_preview_or_provide() {
    let folders = FolderCheck::new(CheckPermissions::Disable);

    assert!(matches!(NoShim.preview(), Err(Error::NoShimProvider)));
    assert!(matches!(NoShim.provide(&folders), Err(Error::NoShimProvider)));
}

#[skuld::test]
fn no_shim_sweeps_nothing_and_succeeds() {
    NoShim.sweep(&[PathBuf::from(r"C:\app\goetia-shim.exe")]).unwrap();
}

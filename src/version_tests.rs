use crate::version;

#[skuld::test]
fn version_is_cargo_pkg_version() {
    assert_eq!(version(), env!("CARGO_PKG_VERSION"));
}

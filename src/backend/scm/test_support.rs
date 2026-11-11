//! Fixtures shared by the SCM unit tests: byte layouts of registry string values, and a service's blob.

/// The bytes goetia stores for a string value: UTF-16LE and one closing NUL.
pub(crate) fn sz(text: &str) -> Vec<u8> {
    units_bytes(&text.encode_utf16().chain([0]).collect::<Vec<_>>())
}

/// `units` as the UTF-16LE bytes a registry value holds.
pub(crate) fn units_bytes(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
}

/// The hash in the file name of a shim host, as a provider places one.
pub(crate) const SHIM_HEX: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

/// A service's blob of `kind`, as `install` writes it for the plain daemon `frpc`.
#[cfg(windows)]
pub(crate) fn blob_of(kind: crate::spec::Kind) -> crate::blob::Blob {
    use crate::spec::{DaemonFields, DaemonSpec};
    use crate::testing::plain_fields;

    crate::blob::Blob {
        schema: 1,
        version: "0.1.0".to_string(),
        spec: DaemonSpec::new(DaemonFields {
            kind,
            ..plain_fields("frpc", vec![crate::spec::test_support::program("frpc")])
        })
        .unwrap(),
    }
}

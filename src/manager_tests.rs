use super::*;

/// `#[cfg]`-gated off Linux since Task 11 gave it a real backend — see
/// `native_succeeds_on_linux` below for that platform's equivalent.
#[cfg(not(target_os = "linux"))]
#[skuld::test]
fn native_names_the_missing_backend() {
    let err = match native() {
        Ok(_) => panic!("no backend is implemented yet on this platform"),
        Err(e) => e,
    };
    let message = err.to_string();

    assert!(
        message.starts_with("no backend for ") && message.ends_with(" yet"),
        "expected \"no backend for <platform> yet\", got: {message}"
    );
    assert!(
        message.contains(std::env::consts::OS),
        "message should name the running platform ({}): {message}",
        std::env::consts::OS
    );
}

#[cfg(target_os = "linux")]
#[skuld::test]
fn native_succeeds_on_linux() {
    native().unwrap_or_else(|e| panic!("native() should return a real backend on linux, got: {e}"));
}

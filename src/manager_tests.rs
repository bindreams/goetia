use super::*;

// macOS has a real backend as of Task 12; linux/windows still don't until
// Tasks 11/13 land.
#[skuld::test]
#[cfg(not(target_os = "macos"))]
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

#[skuld::test]
#[cfg(target_os = "macos")]
fn native_returns_the_launchd_backend() {
    native().expect("macOS has a native() ServiceManager as of Task 12");
}

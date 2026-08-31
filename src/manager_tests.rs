use super::*;

/// Linux has no backend yet (Task 11); macOS and Windows do, as of Tasks 12
/// and 13 — see [`native_returns_the_launchd_backend`]/
/// [`native_returns_a_working_backend_on_windows`].
#[skuld::test]
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
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
    native().expect("macOS has a native() ServiceManager");
}

#[skuld::test]
#[cfg(target_os = "windows")]
fn native_returns_a_working_backend_on_windows() {
    let mgr = native().expect("Windows has a real ServiceManager backend as of Task 13");
    // A cheap, unelevated call: proves `native()` actually wired up
    // `ScmManager` rather than, say, a `Box::new(())`-shaped stub that
    // happens to satisfy the trait's types but panics the instant anything
    // calls it.
    mgr.list().expect("list needs no elevation and no installed services");
}

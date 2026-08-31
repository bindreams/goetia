use super::*;

/// Linux/macOS have no backend yet (Tasks 11-12); Windows does, as of
/// Task 13 — see [`native_returns_a_working_backend_on_windows`].
#[skuld::test]
#[cfg(not(target_os = "windows"))]
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
#[cfg(target_os = "windows")]
fn native_returns_a_working_backend_on_windows() {
    let mgr = native().expect("Windows has a real ServiceManager backend as of Task 13");
    // A cheap, unelevated call: proves `native()` actually wired up
    // `ScmManager` rather than, say, a `Box::new(())`-shaped stub that
    // happens to satisfy the trait's types but panics the instant anything
    // calls it.
    mgr.list().expect("list needs no elevation and no installed services");
}

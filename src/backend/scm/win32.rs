//! Errors from the `windows_service` crate, as the errors the rest of the backend carries.

/// Converts to the `io::Error` callers carry. A `Winapi` failure yields the wrapped OS error, since the
/// variant's own text ("IO error in winapi call") names no reason. Callers that classify by code match
/// on the `windows_service` error first.
pub(crate) fn winapi_io(e: windows_service::Error) -> std::io::Error {
    match e {
        windows_service::Error::Winapi(os) => os,
        other => std::io::Error::other(other),
    }
}

#[cfg(test)]
#[path = "win32_tests.rs"]
mod win32_tests;

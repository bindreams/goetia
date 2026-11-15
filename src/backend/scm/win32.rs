//! Errors from the `windows_service` crate, as the errors the rest of the backend carries.

/// A `windows_service` error as the `io::Error` callers carry: the OS error a `Winapi` failure wraps,
/// whose text is the reason (the variant's own, "IO error in winapi call", names none), and any other
/// variant (`ParseValue`, which the query calls can return) as `io::Error::other`. Callers that
/// classify a failure by its code match on the `windows_service` error before converting it.
pub(crate) fn winapi_io(e: windows_service::Error) -> std::io::Error {
    match e {
        windows_service::Error::Winapi(os) => os,
        other => std::io::Error::other(other),
    }
}

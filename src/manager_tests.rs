use super::*;

#[skuld::test]
fn native_names_the_missing_backend() {
    let err = match native() {
        Ok(_) => panic!("no backend is implemented yet on any platform"),
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

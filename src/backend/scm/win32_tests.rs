use windows_service::service::ServiceStartType;

use super::winapi_io;
use crate::error::Chain;

/// A variant that is not a `Winapi` failure keeps its own text and its cause, and `Chain` prints the
/// cause once.
#[skuld::test]
fn a_variant_other_than_winapi_keeps_its_text_and_its_cause_once() {
    let cause = ServiceStartType::from_raw(99).unwrap_err();
    let cause_text = cause.to_string();
    let e = windows_service::Error::ParseValue("service start type", cause);

    let text = Chain(&winapi_io(e)).to_string();

    assert!(text.starts_with("invalid service start type value"), "{text}");
    assert_eq!(text.matches(&cause_text).count(), 1, "{text}");
}

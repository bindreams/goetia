use super::*;

#[skuld::test]
fn exit_codes_combine_by_precedence() {
    use ExitCode::*;

    assert_eq!(
        [Success, Drift, Conflict, Indeterminate, Error].iter().max(),
        Some(&Error)
    );
    assert_eq!(Drift.max(Conflict), Conflict);
    assert_eq!(Conflict.max(Indeterminate), Indeterminate);
    assert_eq!(Success.max(Drift), Drift);
}

/// The README's exit-code table.
#[skuld::test]
fn exit_code_numbers_are_the_documented_ones() {
    for (code, number) in [
        (ExitCode::Success, 0),
        (ExitCode::Error, 1),
        (ExitCode::Usage, 2),
        (ExitCode::Drift, 3),
        (ExitCode::Indeterminate, 4),
        (ExitCode::Conflict, 5),
    ] {
        assert_eq!(code.code(), number, "{code:?}");
    }
}

#[skuld::test]
fn of_maps_an_error_by_whether_it_left_the_outcome_open() {
    let in_doubt = crate::Error::WaitTimeout {
        id: "x".to_string(),
        awaited: "running",
        waited: std::time::Duration::ZERO,
        recovery: String::new(),
    };
    let failed = crate::Error::Other("failed".to_string());

    assert_eq!(ExitCode::of(&in_doubt), ExitCode::Indeterminate);
    assert_eq!(ExitCode::of(&failed), ExitCode::Error);
}

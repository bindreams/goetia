use super::*;

#[skuld::test]
fn exit_codes_combine_by_precedence() {
    use ExitCode::*;

    assert!(
        [Success, Drift, Conflict, Indeterminate, Error, Usage]
            .windows(2)
            .all(|w| w[0] < w[1])
    );
}

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
    let failed = crate::Error::NotInstalled { id: "x".to_string() };

    assert_eq!(ExitCode::of(&in_doubt), ExitCode::Indeterminate);
    assert_eq!(ExitCode::of(&failed), ExitCode::Error);
}

#[skuld::test]
fn run_exit_code_is_the_programs() {
    use crate::manager::Exit;

    assert_eq!(ExitCode::Run(7).code(), 7);
    assert_eq!(ExitCode::Run(0).code(), 0);
    assert_eq!(ExitCode::of_run(Exit::Code(4)), ExitCode::Run(4));
    assert_eq!(ExitCode::of_run(Exit::Signal(9)), ExitCode::Run(137));
    assert!(ExitCode::Usage < ExitCode::Run(0), "declared last, and never combined");
}

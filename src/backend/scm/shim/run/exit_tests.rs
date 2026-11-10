use std::process::Command;

use super::*;

/// Names the code a child of this test binary ends itself with.
const CODE: &str = "GOETIA_SHIM_EXIT_TEST_CODE";

/// The process ends with exactly the code, whatever its bits: a daemon's NTSTATUS exit is negative as
/// an `i32`. Runs itself in a child that ends itself, as the shim does.
#[skuld::test]
fn end_process_ends_the_process_with_exactly_the_code() {
    if let Ok(code) = std::env::var(CODE) {
        end_process(code.parse().expect("a code"));
    }

    for code in [7, 1003, -1073741819, i32::MIN] {
        let status = Command::new(std::env::current_exe().expect("the test binary"))
            .arg("end_process_ends_the_process_with_exactly_the_code")
            .env(CODE, code.to_string())
            .status()
            .expect("run the child");

        assert_eq!(status.code(), Some(code));
    }
}

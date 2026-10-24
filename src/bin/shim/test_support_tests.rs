use super::*;

#[skuld::test]
fn ids_differing_only_in_sequence_differ() {
    assert_ne!(compose("p", 7, 0), compose("p", 7, 1));
}

#[skuld::test]
fn ids_of_one_prefix_made_in_a_row_differ() {
    let (a, b) = (TestId::new("p"), TestId::new("p"));
    assert_ne!(a.as_str(), b.as_str());
}

fn rejects(lines: &[String], id: &str, expected: &str) -> bool {
    std::panic::catch_unwind(|| assert_one_failure(lines, id, expected)).is_err()
}

#[skuld::test]
fn assert_one_failure_accepts_exactly_the_line() {
    let lines = vec!["goetia-shim[x]: spawn: boom".to_string()];
    assert_one_failure(&lines, "x", "spawn: boom");
}

#[skuld::test]
fn assert_one_failure_rejects_text_appended_to_the_line() {
    let lines = vec!["goetia-shim[x]: spawn: boom, and more".to_string()];
    assert!(rejects(&lines, "x", "spawn: boom"));
}

#[skuld::test]
fn assert_one_failure_rejects_a_line_missing_its_tail() {
    let lines = vec!["goetia-shim[x]: spawn".to_string()];
    assert!(rejects(&lines, "x", "spawn: boom"));
}

#[skuld::test]
fn assert_one_failure_rejects_text_rewritten_in_the_middle() {
    let lines = vec!["goetia-shim[x]: spawn: bam".to_string()];
    assert!(rejects(&lines, "x", "spawn: boom"));
}

#[skuld::test]
fn assert_one_failure_rejects_another_id_and_a_wrong_line_count() {
    let other = vec!["goetia-shim[y]: spawn: boom".to_string()];
    assert!(rejects(&other, "x", "spawn: boom"));
    let two = vec!["goetia-shim[x]: spawn: boom".to_string(); 2];
    assert!(rejects(&two, "x", "spawn: boom"));
    assert!(rejects(&[], "x", "spawn: boom"));
}

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

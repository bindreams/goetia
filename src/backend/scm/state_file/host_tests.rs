use super::*;

#[skuld::test]
fn the_boot_identity_is_a_counter_every_account_can_read_and_it_does_not_change_within_a_boot() {
    let first = clock();
    let second = clock();

    assert_ne!(first.boot, 0, "the kernel numbers boots from 1");
    assert_eq!(first.boot, second.boot);
    assert!(first.ms <= second.ms);
}

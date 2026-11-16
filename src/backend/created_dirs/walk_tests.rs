use super::*;

/// A `..` is taken on the directory held, and the chain says where that is: a directory renamed to
/// another parent between being entered and its `..` being opened leaves a chain that describes a path the
/// walk is no longer on, and no answer from it is to be trusted.
#[skuld::test]
fn dotdot_that_opens_the_directory_the_chain_exposes_pops_it() {
    let mut chain = vec![(1, 1), (1, 2), (1, 3)];

    assert!(step_up(&mut chain, (1, 2)));

    assert_eq!(chain, [(1, 1), (1, 2)]);
}

#[skuld::test]
fn dotdot_that_opens_another_directory_than_the_chain_exposes_is_a_change() {
    let mut chain = vec![(1, 1), (1, 2), (1, 3)];

    assert!(
        !step_up(&mut chain, (7, 9)),
        "the parent is not the one the walk entered from"
    );
}

/// `..` of `/` is `/`.
#[skuld::test]
fn dotdot_at_the_root_stays_at_the_root_and_must_open_it() {
    let mut chain = vec![(1, 1)];

    assert!(step_up(&mut chain, (1, 1)));
    assert_eq!(chain, [(1, 1)]);
    assert!(!step_up(&mut chain, (1, 5)));
}

/// macOS reports a name that changed under the walk as `EAGAIN`, which says nothing of what happened or
/// what to do.
#[skuld::test]
fn a_path_that_changed_under_the_walk_says_so_and_says_to_run_the_install_again() {
    let said = changed_while_read().to_string();

    assert!(said.contains("changed while it was being read"), "{said}");
    assert!(said.contains("running the install again reads it again"), "{said}");
}

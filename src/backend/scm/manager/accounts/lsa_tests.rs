//! `Lsa` against the real account database, read-only, and the sizing loop under it against scripted answers.

use std::io::ErrorKind;

use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER as MORE, ERROR_NONE_MAPPED};

use super::*;
use crate::backend::scm::sid::{sid_from_parts, system};
use crate::elevation::test_support::process_user_sid;

// by_sid ==============================================================================================================

#[skuld::test]
fn a_sid_resolves_to_its_account_name_and_kind() {
    let account = Lsa.by_sid(&system()).expect("LocalSystem always resolves");

    assert!(account.name.contains("SYSTEM"), "{}", account.name);
    assert_eq!(account.sid, system());
    assert_eq!(account.kind, AccountKind::WellKnownGroup);
}

#[skuld::test]
fn bytes_that_are_not_a_sid_are_an_error_not_a_read_past_them() {
    let err = Lsa.by_sid(&vec![1, 3, 0, 0, 0, 0, 0, 5, 1, 0]).expect_err("truncated");

    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert_eq!(err.raw_os_error(), None, "refused before any call");
}

#[skuld::test]
fn a_well_formed_sid_no_account_has_fails_with_none_mapped() {
    let err = Lsa
        .by_sid(&sid_from_parts(5, &[21, 1, 2, 3, 999999]))
        .expect_err("no such account");

    assert_eq!(err.raw_os_error(), Some(ERROR_NONE_MAPPED as i32));
}

// by_name =============================================================================================================

#[skuld::test]
fn a_name_resolves_to_its_sid_and_kind_with_the_name_as_asked() {
    let account = Lsa.by_name("Administrators").expect("Administrators resolves");

    assert_eq!(account.name, "Administrators");
    assert_eq!(account.sid, sid_from_parts(5, &[32, 544]));
    assert_eq!(account.kind, AccountKind::LocalGroup);
}

#[skuld::test]
fn a_leading_dot_backslash_is_stripped_before_the_lookup_and_kept_in_the_name() {
    let account = Lsa
        .by_name(r".\Administrators")
        .expect("`.\\` is SCM's shorthand for this machine");

    assert_eq!(account.name, r".\Administrators");
    assert_eq!(account.sid, sid_from_parts(5, &[32, 544]));
}

#[skuld::test]
fn an_unknown_account_name_fails_with_none_mapped() {
    for name in [
        "goetia-no-such-account-7f3a",
        r".\goetia-no-such-account-7f3a",
        r"GOETIA-NO-SUCH-DOMAIN\nobody",
    ] {
        let err = Lsa.by_name(name).expect_err(name);

        assert_eq!(err.raw_os_error(), Some(ERROR_NONE_MAPPED as i32), "`{name}`: {err}");
    }
}

#[skuld::test]
fn a_nul_in_an_account_name_is_refused_not_cut_there() {
    // Cut at the NUL, this would resolve `SYSTEM`.
    let err = Lsa.by_name("SYSTEM\0junk").expect_err("a NUL ends nothing here");

    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert_eq!(err.raw_os_error(), None, "refused before any call");
}

#[skuld::test]
fn the_account_this_test_runs_as_resolves_back_to_its_own_sid() {
    // Whatever kind of account this is (a user, or `SYSTEM` on a service host), its name resolves to its SID.
    let sid = process_user_sid();
    let name = Lsa.by_sid(&sid).expect("the token user has a name").name;

    let found = Lsa.by_name(&name).expect("the current user resolves by name");

    assert_eq!(found.sid, sid);
}

// AccountKind =========================================================================================================

#[skuld::test]
fn a_users_kind_is_user_and_an_unnamed_kind_is_other() {
    use windows_sys::Win32::Security::{SidTypeInvalid, SidTypeUnknown, SidTypeUser};

    assert_eq!(AccountKind::of(SidTypeUser), AccountKind::User);
    assert_eq!(AccountKind::of(SidTypeUnknown), AccountKind::Other);
    assert_eq!(AccountKind::of(SidTypeInvalid), AccountKind::Other);
}

// lookup_growing ======================================================================================================

/// A lookup call that plays `steps` in order: each sets the two lengths and answers with a status, `0` for success.
fn scripted<const N: usize>(
    steps: [(u32, u32, u32); N],
) -> impl FnMut(*mut u16, &mut u32, *mut u16, &mut u32) -> std::io::Result<()> {
    let mut steps = steps.into_iter();
    move |_first, first_len, _domain, domain_len| {
        let (status, first, domain) = steps.next().expect("the call ran more often than scripted");
        *first_len = first;
        *domain_len = domain;
        if status == 0 {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(status as i32))
        }
    }
}

#[skuld::test]
fn a_lookup_that_needs_more_grows_the_buffers_to_what_it_reported_and_cuts_them_to_what_it_wrote() {
    let call = scripted([(MORE, 10, 4), (0, 3, 2)]);

    let (first, domain) = lookup_growing(call).expect("the second call fits");

    assert_eq!((first.len(), domain.len()), (3, 2));
}

#[skuld::test]
fn a_lookup_that_asks_for_no_more_than_it_has_stops_with_its_own_error() {
    // Asking again with the same lengths would change nothing, whether on the first call or a later one.
    let first_call = lookup_growing(scripted([(MORE, 0, 0)])).expect_err("no progress");
    let later_call = lookup_growing(scripted([(MORE, 5, 3), (MORE, 5, 3)])).expect_err("no progress");
    for err in [first_call, later_call] {
        assert_eq!(err.raw_os_error(), Some(MORE as i32), "{err}");
    }
}

#[skuld::test]
fn a_lookup_that_grows_one_buffer_and_not_the_other_keeps_the_larger() {
    // The second answer needs less of the first buffer than it has, and more of the second. Each call
    // gets the lengths of the buffers it is passed; the first must not shrink between calls.
    let mut seen = Vec::new();
    let mut answers = scripted([(MORE, 10, 0), (MORE, 4, 6), (0, 4, 6)]);
    let call = |first: *mut u16, first_len: &mut u32, domain: *mut u16, domain_len: &mut u32| {
        seen.push((*first_len, *domain_len));
        answers(first, first_len, domain, domain_len)
    };
    let (first, domain) = lookup_growing(call).expect("it fits at last");
    assert_eq!((first.len(), domain.len()), (4, 6));
    assert_eq!(seen, [(0, 0), (10, 0), (10, 6)]);
}

#[skuld::test]
fn a_success_that_wrote_nothing_or_reports_more_than_it_was_given_is_an_error_with_no_os_code() {
    let empty = lookup_growing(scripted([(MORE, 4, 2), (0, 0, 0)])).expect_err("empty");
    let past_first = lookup_growing(scripted([(MORE, 4, 2), (0, 5, 0)])).expect_err("past");
    let past_domain = lookup_growing(scripted([(MORE, 4, 2), (0, 4, 3)])).expect_err("past");
    // Success on the first call, with no buffer: the lengths cannot fit it.
    let unsized_ = lookup_growing(scripted([(0, 1, 0)])).expect_err("past");
    for err in [empty, past_first, past_domain, unsized_] {
        assert_eq!(err.kind(), ErrorKind::Other, "{err}");
        assert_eq!(err.raw_os_error(), None, "{err}");
    }
}

#[skuld::test]
fn a_lookup_that_fails_otherwise_fails_with_that_error() {
    let none_mapped = lookup_growing(scripted([(1332, 0, 0)])).expect_err("no account");
    assert_eq!(none_mapped.raw_os_error(), Some(1332));
    let other = lookup_growing(scripted([(8, 0, 0)])).expect_err("out of memory");
    assert_eq!(other.raw_os_error(), Some(8));
}

#[skuld::test]
fn a_sid_is_cut_to_the_length_its_sub_authority_count_gives() {
    let sid = sid_from_parts(5, &[21, 1, 2]);
    let mut padded = sid.clone();
    padded.extend_from_slice(&[0; 9]);
    assert_eq!(whole_sid_prefix(padded), Some(sid.clone()));
    assert_eq!(whole_sid_prefix(sid[..sid.len() - 1].to_vec()), None);
    assert_eq!(whole_sid_prefix(Vec::new()), None);
    assert_eq!(whole_sid_prefix(vec![1]), None);
    assert_eq!(whole_sid_prefix(vec![2, 0, 0, 0, 0, 0, 0, 5]), None);
}

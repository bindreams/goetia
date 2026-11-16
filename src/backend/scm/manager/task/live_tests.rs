use std::io;

use super::*;
use crate::backend::scm::instance::InstanceId;

fn instance(n: u8, pid: u32) -> Instance {
    Instance {
        id: InstanceId::parse(&format!("{n:032x}")).unwrap(),
        pid,
    }
}

/// A probe that answers from a table of pids and counts what it was asked.
fn probe_of<'a>(alive: &'static [u32], asked: &'a mut Vec<u32>) -> impl FnMut(u32) -> io::Result<Liveness> + 'a {
    move |pid| {
        asked.push(pid);
        Ok(if alive.contains(&pid) {
            Liveness::Alive
        } else {
            Liveness::Gone
        })
    }
}

#[skuld::test]
fn a_listed_instance_whose_process_is_alive_is_kept_and_one_that_is_gone_is_dropped() {
    let listed = vec![instance(1, 100), instance(2, 200)];
    let mut asked = Vec::new();

    let live = live_instances(listed, probe_of(&[200], &mut asked)).unwrap();

    assert_eq!(live, vec![instance(2, 200)]);
    assert_eq!(asked, [100, 200], "each instance is judged by its own pid");
}

// L2: a password-logon user's second start can be accepted before the first run's process has exited.
#[skuld::test]
fn two_live_instances_are_both_returned() {
    let listed = vec![instance(1, 100), instance(2, 200)];
    let mut asked = Vec::new();

    let live = live_instances(listed.clone(), probe_of(&[100, 200], &mut asked)).unwrap();

    assert_eq!(live, listed);
}

#[skuld::test]
fn a_listing_with_nothing_in_it_probes_nothing() {
    let mut asked = Vec::new();

    let live = live_instances(Vec::new(), probe_of(&[], &mut asked)).unwrap();

    assert!(live.is_empty());
    assert!(asked.is_empty());
}

/// An answer that is not "alive" or "gone" is the error: the listing is never proof that a run is
/// alive, and neither is a probe that failed.
#[skuld::test]
fn a_failed_probe_is_the_error_and_never_alive() {
    let listed = vec![instance(1, 100), instance(2, 200)];

    let failed = live_instances(listed, |pid| {
        if pid == 100 {
            Ok(Liveness::Alive)
        } else {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        }
    })
    .expect_err("an instance that could not be judged is no answer");

    assert_eq!(failed.kind(), io::ErrorKind::PermissionDenied);
}

use std::io;
use std::sync::mpsc;
use std::thread;

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_OWNER, ERROR_NOT_ENOUGH_MEMORY};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Threading::MUTEX_MODIFY_STATE;

use super::*;
use crate::testing::groups::{Group, scm_group};

/// Carried by every test that takes the machine-wide lock `Global\goetia-shim`, which wait for each
/// other: one's probe of who holds it must not see another's hold. Shared with
/// `tests/shim_integration` and `tests/scm_integration` by label *name*, which skuld serialises on
/// across processes, as the lock is across them.
#[skuld::label]
const UNIT_DIR_EXCLUSIVE: skuld::Label;

#[skuld::test]
fn the_wide_name_is_the_name() {
    // SAFETY: `NAME_WIDE` is a NUL-terminated static string.
    let units = unsafe { crate::backend::scm::wide::units_until_nul(NAME_WIDE) };

    assert_eq!(String::from_utf16(units).unwrap(), NAME);
}

/// The lock's owner is Administrators, which a token without it enabled (an unelevated one) cannot
/// give an object: `CreateMutexW` fails with `ERROR_INVALID_OWNER`. That is a need for elevation.
#[skuld::test]
fn a_lock_the_token_cannot_own_is_a_need_for_elevation() {
    let error = create_failed(io::Error::from_raw_os_error(ERROR_INVALID_OWNER as i32));

    assert!(matches!(error, Error::ElevationRequired { .. }), "{error:?}");
    assert_eq!(
        error.to_string(),
        "creating or opening the install lock `Global\\goetia-shim` requires elevation (root/Administrator): re-run \
         as root or Administrator"
    );
    assert!(!error.is_indeterminate());
}

/// A caller whose token lacks Administrators that opens a lock an elevated process made is refused
/// by its DACL, which admits only SYSTEM and Administrators. A caller whose token has them, refused,
/// is denied, for a cause the code does not say.
#[skuld::test]
fn access_denied_is_a_need_for_elevation_only_for_a_token_without_administrators() {
    let denied = Some(ERROR_ACCESS_DENIED as i32);

    assert_eq!(failure_kind(denied, false), FailureKind::NeedsElevation);
    assert_eq!(failure_kind(denied, true), FailureKind::Denied);
}

/// A denial for a token with Administrators enabled is told as that, with the owner when it was
/// read: the lock's permissions are one cause, not the established one.
#[skuld::test]
fn a_denied_administrator_is_told_only_what_is_known() {
    let denied = || io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32);

    let readable = failure(denied(), || Ok(true), || Some(r"EVE\eve".to_string()));
    let unreadable = failure(denied(), || Ok(true), || None);

    assert_eq!(
        readable.to_string(),
        "access to the install lock `Global\\goetia-shim`, which is owned by EVE\\eve, was denied to a token with \
         Administrators enabled: either the lock's permissions or the token's integrity level or privileges refuse \
         it. If the lock is someone else's, close every handle to it, by ending the processes that have it open"
    );
    assert_eq!(
        unreadable.to_string(),
        "access to the install lock `Global\\goetia-shim` was denied to a token with Administrators enabled, and its \
         owner could not be read: either the lock's permissions or the token's integrity level or privileges refuse \
         it. If the lock is someone else's, close every handle to it, by ending the processes that have it open"
    );
}

/// A denied token without Administrators needs elevation, and its owner is not read.
#[skuld::test]
fn a_denied_token_without_administrators_needs_elevation() {
    let error = failure(
        io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32),
        || Ok(false),
        || panic!("the owner is read only for a caller the lock shut out"),
    );

    assert!(matches!(error, Error::ElevationRequired { .. }), "{error:?}");
}

/// The token is asked about only for a denial: no other code depends on it.
#[skuld::test]
fn the_token_is_asked_only_for_a_denial() {
    let error = failure(
        io::Error::from_raw_os_error(ERROR_INVALID_OWNER as i32),
        || panic!("the token is not asked about for a code that does not depend on it"),
        || panic!("no owner is read"),
    );

    assert!(matches!(error, Error::ElevationRequired { .. }), "{error:?}");
}

/// A token that cannot be asked leaves the denial and the failure to ask, in that order: neither
/// is guessed at.
#[skuld::test]
fn a_token_that_cannot_be_asked_reports_both_failures() {
    let error = failure(
        io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32),
        || Err(io::Error::other("no token")),
        || panic!("no owner is read"),
    );

    let Error::Both { first, then } = &error else {
        panic!("expected both failures, got {error:?}");
    };
    assert!(matches!(**first, Error::Os { .. }), "{first:?}");
    assert_eq!(
        then.report().to_string(),
        "failed to check whether the token has Administrators enabled: no token"
    );
}

/// Every other code is the OS's, whoever the caller is.
#[skuld::test]
fn any_other_code_is_the_os_failure() {
    for administrators in [false, true] {
        for code in [Some(ERROR_NOT_ENOUGH_MEMORY as i32), Some(1), None] {
            assert_eq!(
                failure_kind(code, administrators),
                FailureKind::Os,
                "{code:?}, administrators {administrators}"
            );
        }
    }
    assert_eq!(
        failure_kind(Some(ERROR_INVALID_OWNER as i32), true),
        FailureKind::NeedsElevation
    );
}

/// The real lock, opened to wait on it, and so kept in existence for as long as the handle lives: a
/// named mutex goes with its last handle, and a probe that opens it by name after that fails.
fn open_lock() -> OwnedHandle {
    // SAFETY: `NAME_WIDE` is NUL-terminated.
    let raw = unsafe { OpenMutexW(SYNCHRONIZE | MUTEX_MODIFY_STATE, 0, NAME_WIDE) };
    assert!(!raw.is_null(), "open the lock: {}", io::Error::last_os_error());
    // SAFETY: a valid handle `OpenMutexW` just returned and nothing else owns.
    unsafe { OwnedHandle::from_raw_handle(raw) }
}

/// The note names the lock and says the command waits for its release: nothing of who holds it, which
/// the waiter does not know.
#[skuld::test]
fn the_note_names_the_lock_and_claims_no_holder() {
    let mut out = Vec::new();

    say_waiting(&mut out);

    assert_eq!(
        String::from_utf8(out).unwrap(),
        "waiting for the install lock `Global\\goetia-shim` to be released\n"
    );
}

/// A write to the given writer that fails (a closed or broken pipe) is ignored; `eprintln!` would panic.
#[skuld::test]
fn a_note_that_cannot_be_written_is_dropped() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
    }

    say_waiting(&mut Broken);
}

/// A mutex of the test's own, with no name and so no system state: [`take`] is the lock's wait, whichever
/// mutex it waits on.
fn private_mutex() -> OwnedHandle {
    // SAFETY: null attributes and name are valid: the mutex is unnamed, not owned by the caller, and
    // goes with its last handle.
    let raw = unsafe { CreateMutexW(std::ptr::null(), 0, std::ptr::null()) };
    assert!(!raw.is_null(), "create a mutex: {}", io::Error::last_os_error());
    // SAFETY: a valid handle `CreateMutexW` just returned and nothing else owns.
    unsafe { OwnedHandle::from_raw_handle(raw) }
}

/// Runs `body` with a holder thread that has taken `mutex` and keeps it until the `release` it is handed
/// is sent on, or dropped (a body that panics lets the holder go, so the test fails instead of
/// hanging). The holder releases the mutex itself: a mutex is released by the thread that holds it.
fn with_holder<R>(mutex: &OwnedHandle, body: impl FnOnce(mpsc::Sender<()>) -> R) -> R {
    let (held, is_held) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    thread::scope(|scope| {
        scope.spawn(move || {
            // SAFETY: `mutex` is a live handle with `SYNCHRONIZE`.
            assert_eq!(
                unsafe { WaitForSingleObject(mutex.as_raw_handle(), INFINITE) },
                WAIT_OBJECT_0
            );
            held.send(()).expect("the test is listening");
            // A sent release and a dropped sender both end the hold.
            let _ = released.recv();
            // SAFETY: this thread holds the mutex, taken above.
            assert_ne!(unsafe { ReleaseMutex(mutex.as_raw_handle()) }, 0);
        });
        is_held.recv().expect("the holder took the mutex");
        body(release)
    })
}

/// A waiter behind a held lock says so once, before it has the lock, and has it when the holder lets go.
/// The holder lets go from inside the note, so the wait ends on that event and on nothing timed.
#[skuld::test]
fn a_waiter_behind_a_held_lock_says_so_once_and_then_takes_it() {
    let mutex = private_mutex();
    let mut said = 0;

    let taken = with_holder(&mutex, |release| {
        take(&mutex, || {
            said += 1;
            release.send(()).expect("the holder is waiting");
        })
    });

    assert!(taken.is_ok(), "{:?}", taken.err());
    assert_eq!(said, 1);
    // SAFETY: the test thread took the mutex in `take`.
    assert_ne!(unsafe { ReleaseMutex(mutex.as_raw_handle()) }, 0);
}

/// A lock nobody holds is taken without a word.
#[skuld::test]
fn a_waiter_that_finds_the_lock_free_says_nothing() {
    let mutex = private_mutex();

    let taken = take(&mutex, || panic!("nothing is waited for"));

    assert!(taken.is_ok(), "{:?}", taken.err());
}

/// A lock whose holder died is the next one's at once: nothing is waited for.
#[skuld::test]
fn a_waiter_that_finds_the_lock_abandoned_says_nothing_and_takes_it() {
    let mutex = private_mutex();
    // A thread that takes it and exits without releasing abandons it.
    thread::scope(|scope| {
        scope
            .spawn(|| {
                take(&mutex, || panic!("nothing is waited for")).unwrap();
            })
            .join()
            .unwrap();
    });

    let taken = take(&mutex, || panic!("an abandoned lock is not waited for"));

    assert!(taken.is_ok(), "{:?}", taken.err());
}

/// A holder that exits without releasing leaves the mutex abandoned, which the next to take it
/// owns all the same.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_lock_whose_holder_exited_without_releasing_it_is_taken(#[fixture(scm_group)] _group: &Group) {
    // Keeps the mutex in existence once the lock that made it is dropped, and until the test ends.
    let _alive = {
        let _created = InstallLock::acquire().unwrap();
        open_lock()
    };
    thread::spawn(|| {
        let held = open_lock();
        // SAFETY: `held` is a live handle with `SYNCHRONIZE`.
        assert_eq!(
            unsafe { WaitForSingleObject(held.as_raw_handle(), INFINITE) },
            WAIT_OBJECT_0
        );
        // Never released: the thread's exit abandons the mutex, which `_alive` keeps in existence.
    })
    .join()
    .unwrap();

    let taken = InstallLock::acquire();

    assert!(taken.is_ok(), "{:?}", taken.err());
}

/// Any other failure is the OS's, as it was.
#[skuld::test]
fn any_other_failure_to_create_the_lock_is_the_os_error() {
    for code in [ERROR_NOT_ENOUGH_MEMORY, 1] {
        let error = create_failed(io::Error::from_raw_os_error(code as i32));

        let Error::Os { what, source, .. } = &error else {
            panic!("expected the OS error, got {error:?}");
        };
        assert!(what.contains(NAME), "{what}");
        assert_eq!(
            source.downcast_ref::<io::Error>().and_then(io::Error::raw_os_error),
            Some(code as i32)
        );
    }
}

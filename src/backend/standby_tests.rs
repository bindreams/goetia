use std::io;
use std::sync::mpsc;
use std::thread;

use super::*;

fn std_thread(name: &str, f: Box<dyn FnOnce() + Send>) -> io::Result<JoinHandle<()>> {
    thread::Builder::new().name(name.to_string()).spawn(f)
}

#[skuld::test]
fn a_handed_share_is_served_on_the_parked_thread() {
    let (served, seen) = mpsc::channel();
    let (standby, thread) = Standby::with("standby-test", std_thread, move |share: u32| {
        served.send(share).unwrap()
    })
    .expect("a thread");

    standby.hand(7).expect("the thread is parked");
    thread.join().expect("the thread ended cleanly");

    assert_eq!(seen.try_recv(), Ok(7));
}

#[skuld::test]
fn a_standby_dropped_unhanded_ends_its_thread_without_serving() {
    let (served, seen) = mpsc::channel();
    let (standby, thread) = Standby::with("standby-test", std_thread, move |share: u32| {
        served.send(share).unwrap()
    })
    .expect("a thread");

    drop(standby);
    thread.join().expect("the thread ended cleanly");

    assert_eq!(seen.try_recv(), Err(mpsc::TryRecvError::Disconnected));
}

#[skuld::test]
fn a_refused_thread_is_the_makers_error_and_serves_nothing() {
    let refused = Standby::<u32>::with(
        "standby-test",
        |_, _| Err(io::Error::other("no thread")),
        |_| panic!("served"),
    )
    .expect_err("the maker refused");

    assert_eq!(refused.to_string(), "no thread");
}

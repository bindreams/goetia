//! MEASUREMENT ONLY, not for merge: what SCM's `NotifyServiceStatusChangeW` delivers and does after a
//! service is marked for deletion, and under load. Nothing here asserts an observation; every run
//! writes its lines through `support::record_probe` (CI uploads `probe-results/`).
//!
//! Questions (unit `scm-delivery-measure`):
//! 1. A registration pending when the service is marked for deletion: what `dwNotificationStatus`
//!    does its delivery carry?
//! 2. After a delivery, re-registering on the same handle (same buffer, new buffer) or on a fresh
//!    handle: what does the call return, and does it fire?
//! 3. Under load (many handles on one SCM connection, a thread that is not draining its APCs, a slow
//!    callback), does any delivery carry `ERROR_SERVICE_NOTIFY_CLIENT_LAGGING`, and does it repeat on
//!    re-arm?
//!
//! Waits: every wait ends on the delivery it waits for, or on a failure bound that is printed as
//! `silent <n>ms`; such a line says no delivery arrived within that bound, not that none ever will.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::fmt::Write as _;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use goetia::backend::scm::manager::Scm;
use goetia::manager::{Budget, ServiceManager as _};
use goetia::spec::{DaemonSpec, ServiceSpec};
use windows_service::service::{Service, ServiceAccess};
use windows_service::service_manager::{ServiceManager as WinScm, ServiceManagerAccess};
use windows_sys::Win32::System::Services::{
    NotifyServiceStatusChangeW, SERVICE_NOTIFY_2W, SERVICE_NOTIFY_DELETE_PENDING, SERVICE_NOTIFY_DELETED,
    SERVICE_NOTIFY_RUNNING, SERVICE_NOTIFY_STATUS_CHANGE, SERVICE_NOTIFY_STOPPED,
};
use windows_sys::Win32::System::Threading::SleepEx;

use crate::common::fixture_command;
use crate::support::{self, ConnectBack, Group, ServiceGuard, scm_group};

/// STOPPED, START_PENDING, STOP_PENDING, RUNNING, CONTINUE_PENDING, PAUSE_PENDING, PAUSED.
const FULL: u32 = 0x7F;
const DELETE_BITS: u32 = SERVICE_NOTIFY_DELETED | SERVICE_NOTIFY_DELETE_PENDING;
/// How long a delivery that may never come is waited for, before the line says `silent`.
const BOUND: Duration = Duration::from_secs(3);
/// How long the drain of a load run waits for one more delivery, before it counts the rest as missing.
const QUIET: Duration = Duration::from_secs(5);
const ITERATIONS: usize = 18;

// Deliveries ----------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Seen {
    index: usize,
    status: u32,
    triggered: u32,
    state: u32,
}

impl Seen {
    fn show(&self) -> String {
        format!(
            "status={} triggered={:#x} state={}",
            self.status, self.triggered, self.state
        )
    }
}

/// Where the callback of every registration made against it records its delivery.
struct Sink {
    log: Mutex<Vec<Seen>>,
    callback_delay: Duration,
}

impl Sink {
    fn leak(callback_delay: Duration) -> &'static Sink {
        Box::leak(Box::new(Sink {
            log: Mutex::new(Vec::new()),
            callback_delay,
        }))
    }

    fn log(&self) -> MutexGuard<'_, Vec<Seen>> {
        self.log.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn len(&self) -> usize {
        self.log().len()
    }
}

struct Tag {
    sink: &'static Sink,
    index: usize,
}

unsafe extern "system" fn on_delivery(p: *const c_void) {
    // SAFETY: SCM hands back the `SERVICE_NOTIFY_2W` that `Registration::arm` registered, whose
    // `pContext` is a leaked `Tag`; neither is ever freed.
    let (n, tag) = unsafe {
        let n = &*p.cast::<SERVICE_NOTIFY_2W>();
        (n, &*n.pContext.cast::<Tag>())
    };
    if !tag.sink.callback_delay.is_zero() {
        std::thread::sleep(tag.sink.callback_delay);
    }
    tag.sink.log().push(Seen {
        index: tag.index,
        status: n.dwNotificationStatus,
        triggered: n.dwNotificationTriggered,
        state: n.ServiceStatus.dwCurrentState,
    });
}

#[derive(Clone, Copy)]
enum Buffer {
    Same,
    New,
}

/// One registration's buffer. Leaked, never freed: a registration may outlive any scope here.
struct Registration {
    notify: &'static mut SERVICE_NOTIFY_2W,
    tag: &'static Tag,
}

impl Registration {
    fn new(sink: &'static Sink, index: usize) -> Self {
        Self {
            notify: Box::leak(Box::default()),
            tag: Box::leak(Box::new(Tag { sink, index })),
        }
    }

    /// `NotifyServiceStatusChangeW`'s return code. The buffer is rewritten as the product's arm does
    /// (`Buffer::Same`), or replaced by a new one (`Buffer::New`).
    fn arm(&mut self, svc: &Service, mask: u32, buffer: Buffer) -> u32 {
        if matches!(buffer, Buffer::New) {
            self.notify = Box::leak(Box::default());
        }
        *self.notify = SERVICE_NOTIFY_2W {
            dwVersion: SERVICE_NOTIFY_STATUS_CHANGE,
            pfnNotifyCallback: Some(on_delivery),
            pContext: std::ptr::from_ref(self.tag).cast_mut().cast(),
            ..Default::default()
        };
        // SAFETY: the handle has `SERVICE_QUERY_STATUS`; the buffer is leaked, so it outlives the registration.
        unsafe { NotifyServiceStatusChangeW(svc.raw_handle(), mask, std::ptr::from_mut(&mut *self.notify)) }
    }
}

/// The delivery number `seen` once it arrives, or `None` after `bound` with none.
fn await_delivery(sink: &Sink, seen: usize, bound: Duration) -> Option<Seen> {
    let start = Instant::now();
    loop {
        if let Some(d) = sink.log().get(seen) {
            return Some(*d);
        }
        let left = bound.checked_sub(start.elapsed())?;
        // SAFETY: an alertable wait; the callback runs inside it.
        unsafe { SleepEx(left.as_millis().max(1) as u32, 1) };
    }
}

fn show_delivery(d: Option<Seen>, bound: Duration) -> String {
    match d {
        Some(d) => d.show(),
        None => format!("silent {}ms", bound.as_millis()),
    }
}

/// Drains until `want` deliveries are in, or until `quiet` passes with no delivery.
fn drain_until(sink: &Sink, want: usize, quiet: Duration) -> usize {
    while sink.len() < want {
        // SAFETY: an alertable wait; the callbacks run inside it. 0 means `quiet` elapsed with none.
        if unsafe { SleepEx(quiet.as_millis() as u32, 1) } == 0 {
            break;
        }
    }
    sink.len()
}

fn code<T>(r: Result<T, windows_service::Error>) -> String {
    match r {
        Ok(_) => "ok".to_string(),
        Err(windows_service::Error::Winapi(e)) => format!("err {}", e.raw_os_error().unwrap_or(-1)),
        Err(e) => format!("err {e}"),
    }
}

fn open_scm() -> WinScm {
    WinScm::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).expect("open the SCM")
}

const ACCESS: ServiceAccess = ServiceAccess::QUERY_STATUS
    .union(ServiceAccess::STOP)
    .union(ServiceAccess::DELETE);

fn installed_fixture(mgr: &Scm, id: &str, started: &ConnectBack, stopped: &ConnectBack, start: bool) {
    let spec = ServiceSpec::new(support::base_fields(
        id,
        fixture_command(id, started.port(), stopped.port(), "plain"),
    ))
    .map(DaemonSpec::from)
    .unwrap();
    mgr.install(&spec, false).expect("install");
    if start {
        mgr.start(&spec.common().id, Budget::DEFAULT).expect("start");
        started.accept("the fixture to report SERVICE_RUNNING");
    }
}

// Q1 and Q2: one registration, a delete, a re-arm ---------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Mask {
    /// The mask of the first registration: everything but the state the service is in.
    Excluding,
    /// Everything, the current state included.
    Including,
}

#[derive(Clone, Copy)]
enum Follow {
    SameHandle(Buffer, Mask),
    FreshHandle(Mask),
}

impl Follow {
    fn name(self) -> String {
        let mask = |m| match m {
            Mask::Excluding => "mask-excluding-current",
            Mask::Including => "mask-including-current",
        };
        match self {
            Follow::SameHandle(Buffer::Same, m) => format!("same-handle/same-buffer/{}", mask(m)),
            Follow::SameHandle(Buffer::New, m) => format!("same-handle/new-buffer/{}", mask(m)),
            Follow::FreshHandle(m) => format!("fresh-handle/{}", mask(m)),
        }
    }
}

const FOLLOWS: [Follow; 6] = [
    Follow::SameHandle(Buffer::Same, Mask::Excluding),
    Follow::SameHandle(Buffer::Same, Mask::Including),
    Follow::SameHandle(Buffer::New, Mask::Excluding),
    Follow::SameHandle(Buffer::New, Mask::Including),
    Follow::FreshHandle(Mask::Excluding),
    Follow::FreshHandle(Mask::Including),
];

/// A registration pending on `h`, `DeleteService` through `h`, then (for a running service) a stop,
/// then one re-arm by `follow`. Returns what was seen, one line.
fn one_iteration(running: bool, extra: u32, follow: Follow) -> String {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let (started, stopped) = (ConnectBack::listen(), ConnectBack::listen());
    installed_fixture(&mgr, &id, &started, &stopped, running);

    let scm = open_scm();
    let h = scm.open_service(&id, ACCESS).expect("open the service");
    let current = if running {
        SERVICE_NOTIFY_RUNNING
    } else {
        SERVICE_NOTIFY_STOPPED
    };
    let first = (FULL & !current) | extra;
    let sink = Sink::leak(Duration::ZERO);
    let mut reg = Registration::new(sink, 0);
    let mut line = String::new();

    let _ = write!(line, "arm rc={}; ", reg.arm(&h, first, Buffer::Same));
    let _ = write!(line, "DeleteService {}; ", code(h.delete()));
    let mut d = await_delivery(sink, 0, BOUND);
    let _ = write!(line, "delivery after the mark: {}; ", show_delivery(d, BOUND));
    if running && d.is_none() {
        let stop = h.stop();
        let stop_ok = stop.is_ok();
        let _ = write!(line, "STOP {}; ", code(stop));
        if stop_ok {
            stopped.accept("the fixture to handle SERVICE_CONTROL_STOP");
            d = await_delivery(sink, 0, BOUND);
            let _ = write!(line, "delivery after the stop: {}; ", show_delivery(d, BOUND));
        }
    }
    let _ = write!(
        line,
        "state {}; ",
        code(h.query_status().map(|s| s.current_state as u32))
    );

    let again = |m| match m {
        Mask::Excluding => first,
        Mask::Including => FULL | extra,
    };
    let next = sink.len();
    match follow {
        Follow::SameHandle(buffer, m) => {
            let rc = reg.arm(&h, again(m), buffer);
            let d2 = await_delivery(sink, next, BOUND);
            let _ = write!(line, "re-arm rc={rc}: {}", show_delivery(d2, BOUND));
        }
        Follow::FreshHandle(m) => match scm.open_service(&id, ACCESS) {
            Ok(h2) => {
                let mut reg2 = Registration::new(sink, 1);
                let rc = reg2.arm(&h2, again(m), Buffer::Same);
                let d2 = await_delivery(sink, next, BOUND);
                let _ = write!(line, "fresh handle: re-arm rc={rc}: {}", show_delivery(d2, BOUND));
            }
            Err(e) => {
                let _ = write!(line, "fresh handle: open {}", code::<()>(Err(e)));
            }
        },
    }
    line
}

fn run_cell(name: &str, running: bool, extra: u32) {
    let mut lines = String::new();
    let mut tally: BTreeMap<String, usize> = BTreeMap::new();
    for i in 0..ITERATIONS {
        let follow = FOLLOWS[i % FOLLOWS.len()];
        let line = format!("{}: {}", follow.name(), one_iteration(running, extra, follow));
        let _ = writeln!(lines, "iteration {i}: {line}");
        *tally.entry(line).or_default() += 1;
    }
    let mut body = format!(
        "service {}; first mask = all states but the current{}; {ITERATIONS} iterations, each on its own service; \
         bound for a delivery that may not come: {}ms\n\n",
        if running { "RUNNING" } else { "STOPPED" },
        if extra == 0 {
            ""
        } else {
            " plus DELETED and DELETE_PENDING"
        },
        BOUND.as_millis()
    );
    body.push_str("tally (count, line):\n");
    for (line, count) in &tally {
        let _ = writeln!(body, "{count:>3}x {line}");
    }
    body.push_str("\nevery iteration:\n");
    body.push_str(&lines);
    support::record_probe(name, &body);
}

#[skuld::test]
fn delivery_probe_stopped_service(#[fixture(scm_group)] _group: &Group) {
    run_cell("scm-delivery-stopped", false, 0);
}

#[skuld::test]
fn delivery_probe_stopped_service_delete_bits(#[fixture(scm_group)] _group: &Group) {
    run_cell("scm-delivery-stopped-delete-bits", false, DELETE_BITS);
}

#[skuld::test]
fn delivery_probe_running_service(#[fixture(scm_group)] _group: &Group) {
    run_cell("scm-delivery-running", true, 0);
}

#[skuld::test]
fn delivery_probe_running_service_delete_bits(#[fixture(scm_group)] _group: &Group) {
    run_cell("scm-delivery-running-delete-bits", true, DELETE_BITS);
}

// Q3: many handles on one SCM connection, a thread that is not draining -----------------------------------------------

#[derive(Clone, Copy)]
enum Trigger {
    Stop,
    Delete,
}

fn tally_of<'a>(seen: impl Iterator<Item = &'a Seen>) -> String {
    let mut by: BTreeMap<String, usize> = BTreeMap::new();
    for s in seen {
        *by.entry(s.show()).or_default() += 1;
    }
    format!("{by:?}")
}

fn rc_tally(rcs: impl Iterator<Item = u32>) -> String {
    let mut by: BTreeMap<u32, usize> = BTreeMap::new();
    for rc in rcs {
        *by.entry(rc).or_default() += 1;
    }
    format!("{by:?}")
}

/// `handles` handles on one SCM connection to a running service, each with a registration; this thread
/// does not drain while the service is stopped (or deleted) through the first; then everything is drained
/// and every handle whose delivery failed is re-armed on its handle and on a handle of a new connection.
fn one_round(handles: usize, delay: Duration, trigger: Trigger, extra: u32) -> String {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let (started, stopped) = (ConnectBack::listen(), ConnectBack::listen());
    installed_fixture(&mgr, &id, &started, &stopped, true);

    let scm = open_scm();
    let services: Vec<Service> = (0..handles)
        .map(|_| scm.open_service(&id, ACCESS).expect("open a handle"))
        .collect();
    let sink = Sink::leak(delay);
    let mask = (FULL & !SERVICE_NOTIFY_RUNNING) | extra;
    let mut regs: Vec<Registration> = (0..handles).map(|i| Registration::new(sink, i)).collect();
    let arm_rcs: Vec<u32> = regs
        .iter_mut()
        .zip(&services)
        .map(|(r, s)| r.arm(s, mask, Buffer::Same))
        .collect();
    let mut line = format!(
        "{handles} handles, callback delay {}ms; arm rcs {}; ",
        delay.as_millis(),
        rc_tally(arm_rcs.into_iter())
    );

    match trigger {
        Trigger::Stop => {
            let _ = write!(line, "STOP {}; ", code(services[0].stop()));
            stopped.accept("the fixture to handle SERVICE_CONTROL_STOP");
        }
        Trigger::Delete => {
            let _ = write!(line, "DeleteService {}; ", code(services[0].delete()));
        }
    }
    let got = drain_until(sink, handles, QUIET);
    let _ = write!(
        line,
        "delivered {got} of {handles} (quiet bound {}ms): {}; ",
        QUIET.as_millis(),
        tally_of(sink.log().iter())
    );

    let failed: Vec<usize> = sink.log().iter().filter(|s| s.status != 0).map(|s| s.index).collect();
    if failed.is_empty() {
        line.push_str("no failed delivery");
        return line;
    }
    let before = sink.len();
    let rcs: Vec<u32> = failed
        .iter()
        .map(|&i| regs[i].arm(&services[i], FULL | extra, Buffer::Same))
        .collect();
    let again = drain_until(sink, before + failed.len(), QUIET);
    let _ = write!(
        line,
        "re-armed {} failed on the same handle: rcs {}; delivered {}: {}; ",
        failed.len(),
        rc_tally(rcs.into_iter()),
        again - before,
        tally_of(sink.log().iter().skip(before))
    );

    let fresh_scm = open_scm();
    let sample = &failed[..failed.len().min(10)];
    let fresh: Vec<Service> = sample
        .iter()
        .map(|_| {
            fresh_scm
                .open_service(&id, ACCESS)
                .expect("open a handle on a new connection")
        })
        .collect();
    let before = sink.len();
    let mut fresh_regs: Vec<Registration> = sample.iter().map(|&i| Registration::new(sink, i)).collect();
    let rcs: Vec<u32> = fresh_regs
        .iter_mut()
        .zip(&fresh)
        .map(|(r, s)| r.arm(s, FULL | extra, Buffer::Same))
        .collect();
    let again = drain_until(sink, before + sample.len(), QUIET);
    let _ = write!(
        line,
        "{} of them on a new connection: rcs {}; delivered {}: {}",
        sample.len(),
        rc_tally(rcs.into_iter()),
        again - before,
        tally_of(sink.log().iter().skip(before))
    );
    line
}

fn run_load(name: &str, configs: &[(usize, Duration, Trigger, u32, usize)]) {
    let mut body = String::from(
        "one running fixture service, N handles on one SCM connection, a registration on each, \
         this thread not draining while the trigger runs\n\n",
    );
    for &(handles, delay, trigger, extra, rounds) in configs {
        for round in 0..rounds {
            let line = one_round(handles, delay, trigger, extra);
            let _ = writeln!(
                body,
                "round {round}, trigger {}, mask extra {extra:#x}: {line}",
                if matches!(trigger, Trigger::Stop) {
                    "stop"
                } else {
                    "delete"
                }
            );
        }
    }
    support::record_probe(name, &body);
}

#[skuld::test]
fn delivery_probe_load_fast_callback(#[fixture(scm_group)] _group: &Group) {
    let none = Duration::ZERO;
    run_load(
        "scm-delivery-load-fast",
        &[
            (100, none, Trigger::Stop, 0, 3),
            (1000, none, Trigger::Stop, 0, 3),
            (4000, none, Trigger::Stop, 0, 2),
        ],
    );
}

#[skuld::test]
fn delivery_probe_load_slow_callback(#[fixture(scm_group)] _group: &Group) {
    let slow = Duration::from_millis(5);
    run_load(
        "scm-delivery-load-slow",
        &[
            (100, slow, Trigger::Stop, 0, 3),
            (1000, slow, Trigger::Stop, 0, 3),
            (3000, slow, Trigger::Stop, 0, 2),
        ],
    );
}

#[skuld::test]
fn delivery_probe_load_delete(#[fixture(scm_group)] _group: &Group) {
    run_load(
        "scm-delivery-load-delete",
        &[
            (1000, Duration::ZERO, Trigger::Delete, DELETE_BITS, 3),
            (1000, Duration::from_millis(5), Trigger::Delete, DELETE_BITS, 3),
        ],
    );
}

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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
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
const DELETE_PENDING: u32 = SERVICE_NOTIFY_DELETE_PENDING;
/// How long a delivery that may never come is waited for, before the line says `silent`.
const BOUND: Duration = Duration::from_secs(1);
/// How long the drain of a load run waits for one more delivery, before it counts the rest as missing.
const QUIET: Duration = Duration::from_secs(2);
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

/// What the first steps of an iteration do.
#[derive(Clone, Copy)]
struct Shape {
    /// A registration is pending before the delete and the stop.
    arm_first: bool,
    /// The service is marked for deletion.
    delete: bool,
}

/// A registration pending on `h` (if `shape.arm_first`), `DeleteService` through `h` (if `shape.delete`), then
/// (for a running service) a stop, then one arm by `follow`. Returns what was seen, one line.
fn one_iteration(running: bool, extra: u32, shape: Shape, follow: Follow) -> String {
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

    if shape.arm_first {
        let _ = write!(line, "arm rc={}; ", reg.arm(&h, first, Buffer::Same));
    }
    if shape.delete {
        let _ = write!(line, "DeleteService {}; ", code(h.delete()));
        if shape.arm_first {
            let d = await_delivery(sink, 0, BOUND);
            let _ = write!(line, "delivery after the mark: {}; ", show_delivery(d, BOUND));
        }
    }
    if running && sink.len() == 0 {
        let stop = h.stop();
        let stop_ok = stop.is_ok();
        let _ = write!(line, "STOP {}; ", code(stop));
        if stop_ok {
            stopped.accept("the fixture to handle SERVICE_CONTROL_STOP");
            if shape.arm_first {
                let d = await_delivery(sink, 0, BOUND);
                let _ = write!(line, "delivery after the stop: {}; ", show_delivery(d, BOUND));
            }
        }
    }
    let _ = match h.query_status() {
        Ok(s) => write!(line, "state {}; ", s.current_state as u32),
        Err(e) => write!(line, "state {}; ", code::<()>(Err(e))),
    };

    let again = |m| match m {
        Mask::Excluding => first,
        Mask::Including => FULL | extra,
    };
    let next = sink.len();
    match follow {
        Follow::SameHandle(buffer, m) => {
            let rc = reg.arm(&h, again(m), buffer);
            let d2 = await_delivery(sink, next, BOUND);
            let _ = write!(line, "arm again rc={rc}: {}", show_delivery(d2, BOUND));
        }
        Follow::FreshHandle(m) => match scm.open_service(&id, ACCESS) {
            Ok(h2) => {
                let mut reg2 = Registration::new(sink, 1);
                let rc = reg2.arm(&h2, again(m), Buffer::Same);
                let d2 = await_delivery(sink, next, BOUND);
                let _ = write!(line, "fresh handle: arm rc={rc}: {}", show_delivery(d2, BOUND));
            }
            Err(e) => {
                let _ = write!(line, "fresh handle: open {}", code::<()>(Err(e)));
            }
        },
    }
    line
}

fn run_cell(name: &str, running: bool, extra: u32, shape: Shape, iterations: usize) {
    let mut lines = String::new();
    let mut tally: BTreeMap<String, usize> = BTreeMap::new();
    for i in 0..iterations {
        let follow = FOLLOWS[i % FOLLOWS.len()];
        let line = format!("{}: {}", follow.name(), one_iteration(running, extra, shape, follow));
        let _ = writeln!(lines, "iteration {i}: {line}");
        *tally.entry(line).or_default() += 1;
    }
    let mut body = format!(
        "service {}; first mask = all states but the current{}; registration pending first: {}; marked for \
         deletion: {}; {iterations} iterations, each on its own service; bound for a delivery that may not come: \
         {}ms\n\n",
        if running { "RUNNING" } else { "STOPPED" },
        if extra == 0 { "" } else { " plus DELETE_PENDING" },
        shape.arm_first,
        shape.delete,
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

const BOTH: Shape = Shape {
    arm_first: true,
    delete: true,
};

#[skuld::test]
fn delivery_probe_stopped_service(#[fixture(scm_group)] _group: &Group) {
    run_cell("scm-delivery-stopped", false, 0, BOTH, ITERATIONS);
}

#[skuld::test]
fn delivery_probe_stopped_service_delete_pending(#[fixture(scm_group)] _group: &Group) {
    run_cell(
        "scm-delivery-stopped-delete-pending",
        false,
        DELETE_PENDING,
        BOTH,
        ITERATIONS,
    );
}

#[skuld::test]
fn delivery_probe_running_service(#[fixture(scm_group)] _group: &Group) {
    run_cell("scm-delivery-running", true, 0, BOTH, ITERATIONS);
}

#[skuld::test]
fn delivery_probe_running_service_delete_pending(#[fixture(scm_group)] _group: &Group) {
    run_cell(
        "scm-delivery-running-delete-pending",
        true,
        DELETE_PENDING,
        BOTH,
        ITERATIONS,
    );
}

/// The same flows with the mark or the first registration left out, to say which of them a result belongs to.
#[skuld::test]
fn delivery_probe_controls(#[fixture(scm_group)] _group: &Group) {
    let shapes = [
        (
            "registered-not-marked",
            Shape {
                arm_first: true,
                delete: false,
            },
        ),
        (
            "marked-not-registered",
            Shape {
                arm_first: false,
                delete: true,
            },
        ),
        (
            "neither",
            Shape {
                arm_first: false,
                delete: false,
            },
        ),
    ];
    for (label, shape) in shapes {
        for (state, running) in [("stopped", false), ("running", true)] {
            run_cell(
                &format!("scm-delivery-control-{state}-{label}"),
                running,
                0,
                shape,
                FOLLOWS.len(),
            );
        }
    }
}

/// `NotifyServiceStatusChangeW`'s return for single mask bits, on a service handle.
#[skuld::test]
fn delivery_probe_which_mask_bits_a_service_handle_takes(#[fixture(scm_group)] _group: &Group) {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let (started, stopped) = (ConnectBack::listen(), ConnectBack::listen());
    installed_fixture(&mgr, &id, &started, &stopped, false);
    let scm = open_scm();
    let h = scm.open_service(&id, ACCESS).expect("open the service");
    let sink = Sink::leak(Duration::ZERO);
    let mut body = String::from("rc of NotifyServiceStatusChangeW on a STOPPED service's handle, by mask\n");
    for (name, mask) in [
        ("CREATED 0x80", 0x80),
        ("DELETED 0x100", SERVICE_NOTIFY_DELETED),
        ("DELETE_PENDING 0x200", SERVICE_NOTIFY_DELETE_PENDING),
        (
            "DELETED | DELETE_PENDING",
            SERVICE_NOTIFY_DELETED | SERVICE_NOTIFY_DELETE_PENDING,
        ),
    ] {
        let mut reg = Registration::new(sink, 0);
        let _ = writeln!(body, "{name}: rc {}", reg.arm(&h, mask, Buffer::Same));
    }
    support::record_probe("scm-delivery-mask-bits", &body);
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
    let slow = Duration::from_millis(2);
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
            (1000, Duration::ZERO, Trigger::Delete, DELETE_PENDING, 3),
            (1000, Duration::from_millis(2), Trigger::Delete, DELETE_PENDING, 3),
        ],
    );
}

// Q3b: a stream of state changes, re-armed from the callback -----------------------------------------------------------

/// What the callbacks of a stream of re-arming registrations tally.
struct Stream {
    by_status: Mutex<BTreeMap<u32, u64>>,
    /// The status of a delivery that follows a failed one on the same registration.
    after_failure: Mutex<BTreeMap<u32, u64>>,
    rearm_rc: Mutex<BTreeMap<u32, u64>>,
    live: AtomicBool,
    callback_delay: Duration,
}

struct StreamTag {
    stream: &'static Stream,
    service: usize,
    previous_failed: AtomicBool,
}

fn bump(map: &Mutex<BTreeMap<u32, u64>>, key: u32) {
    *map.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(key)
        .or_default() += 1;
}

/// Tallies the delivery and re-arms the same registration, as `P17` did and the product's wait does not: with
/// every state but the delivered one, or every state after a failure. No lock is held across the re-arm, which
/// is an RPC that a nested APC may interrupt.
unsafe extern "system" fn on_stream(p: *const c_void) {
    let buf = p.cast::<SERVICE_NOTIFY_2W>().cast_mut();
    // SAFETY: SCM hands back the buffer `arm_stream` registered, whose `pContext` is a leaked `StreamTag`.
    let (status, state, context, tag) = unsafe {
        let n = &*buf;
        (
            n.dwNotificationStatus,
            n.ServiceStatus.dwCurrentState,
            n.pContext,
            &*n.pContext.cast::<StreamTag>(),
        )
    };
    let stream = tag.stream;
    if !stream.callback_delay.is_zero() {
        std::thread::sleep(stream.callback_delay);
    }
    bump(&stream.by_status, status);
    if tag.previous_failed.swap(status != 0, Ordering::Relaxed) {
        bump(&stream.after_failure, status);
    }
    if !stream.live.load(Ordering::Acquire) {
        return;
    }
    let mask = if status == 0 && (1..=7).contains(&state) {
        FULL & !(1 << (state - 1))
    } else {
        FULL
    };
    // SAFETY: the buffer is this registration's own and leaked; it is rewritten as `Registration::arm` does.
    let rc = unsafe {
        *buf = SERVICE_NOTIFY_2W {
            dwVersion: SERVICE_NOTIFY_STATUS_CHANGE,
            pfnNotifyCallback: Some(on_stream),
            pContext: context,
            ..Default::default()
        };
        NotifyServiceStatusChangeW(tag.service as *mut c_void, mask, buf)
    };
    bump(&stream.rearm_rc, rc);
}

fn arm_stream(stream: &'static Stream, svc: &Service, mask: u32) -> u32 {
    let tag: &'static StreamTag = Box::leak(Box::new(StreamTag {
        stream,
        service: svc.raw_handle() as usize,
        previous_failed: AtomicBool::new(false),
    }));
    let buf: &'static mut SERVICE_NOTIFY_2W = Box::leak(Box::new(SERVICE_NOTIFY_2W {
        dwVersion: SERVICE_NOTIFY_STATUS_CHANGE,
        pfnNotifyCallback: Some(on_stream),
        pContext: std::ptr::from_ref(tag).cast_mut().cast(),
        ..Default::default()
    }));
    // SAFETY: the handle has `SERVICE_QUERY_STATUS`; the buffer is leaked.
    unsafe { NotifyServiceStatusChangeW(svc.raw_handle(), mask, std::ptr::from_mut(buf)) }
}

fn show_map(map: &Mutex<BTreeMap<u32, u64>>) -> String {
    format!("{:?}", *map.lock().unwrap_or_else(PoisonError::into_inner))
}

/// `handles` handles on one SCM connection, each re-arming itself from its callback, while the fixture reports
/// `PAUSED` and `RUNNING` `cycles` times in a row. This thread drains throughout.
fn one_flap(handles: usize, cycles: u32, delay: Duration) -> String {
    use std::io::Read as _;
    use std::net::Shutdown;

    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let (started, stopped) = (ConnectBack::listen(), ConnectBack::listen());
    let spec = ServiceSpec::new(support::base_fields(
        &id,
        fixture_command(&id, started.port(), stopped.port(), &format!("flap:{cycles}")),
    ))
    .map(DaemonSpec::from)
    .unwrap();
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.common().id, Budget::DEFAULT).expect("start");
    let mut parked = started.accept_stream("the fixture, RUNNING and waiting to flap");
    let mut pid = String::new();
    parked.read_to_string(&mut pid).expect("the fixture's pid");

    let scm = open_scm();
    let services: Vec<Service> = (0..handles)
        .map(|_| scm.open_service(&id, ACCESS).expect("open a handle"))
        .collect();
    let stream: &'static Stream = Box::leak(Box::new(Stream {
        by_status: Mutex::default(),
        after_failure: Mutex::default(),
        rearm_rc: Mutex::default(),
        live: AtomicBool::new(true),
        callback_delay: delay,
    }));
    let arm_rcs: Vec<u32> = services
        .iter()
        .map(|s| arm_stream(stream, s, FULL & !SERVICE_NOTIFY_RUNNING))
        .collect();

    let done = Arc::new(AtomicBool::new(false));
    let waiter = {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            stopped.accept_unbounded();
            done.store(true, Ordering::Release);
        })
    };
    parked.shutdown(Shutdown::Write).expect("release the fixture to flap");
    // An alertable wait that re-checks the real condition, the fixture's report that it is done, each time.
    while !done.load(Ordering::Acquire) {
        // SAFETY: an alertable wait; the callbacks run inside it.
        unsafe { SleepEx(100, 1) };
    }
    waiter.join().expect("the waiter");
    // SAFETY: as above. 0 means `QUIET` passed with no delivery.
    while unsafe { SleepEx(QUIET.as_millis() as u32, 1) } != 0 {}
    stream.live.store(false, Ordering::Release);
    // SAFETY: as above; runs what is still queued.
    while unsafe { SleepEx(0, 1) } != 0 {}

    let mut rcs: BTreeMap<u32, usize> = BTreeMap::new();
    for rc in arm_rcs {
        *rcs.entry(rc).or_default() += 1;
    }
    format!(
        "{handles} handles, {cycles} cycles, callback delay {}ms; first arm rcs {rcs:?}; deliveries by status {}; \
         by status after a failed delivery {}; re-arm rcs {}",
        delay.as_millis(),
        show_map(&stream.by_status),
        show_map(&stream.after_failure),
        show_map(&stream.rearm_rc),
    )
}

fn run_flaps(name: &str, configs: &[(usize, u32, Duration, usize)]) {
    let mut body = String::from(
        "one running fixture service reporting PAUSED and RUNNING in a row; N handles on one SCM connection, each \
         re-arming itself from its callback; this thread drains throughout\n\n",
    );
    for &(handles, cycles, delay, rounds) in configs {
        for round in 0..rounds {
            let _ = writeln!(body, "round {round}: {}", one_flap(handles, cycles, delay));
        }
    }
    support::record_probe(name, &body);
}

#[skuld::test]
fn delivery_probe_flap_fast_callback(#[fixture(scm_group)] _group: &Group) {
    let none = Duration::ZERO;
    run_flaps("scm-delivery-flap-fast", &[(50, 300, none, 2), (500, 100, none, 2)]);
}

#[skuld::test]
fn delivery_probe_flap_slow_callback(#[fixture(scm_group)] _group: &Group) {
    let slow = Duration::from_millis(2);
    run_flaps("scm-delivery-flap-slow", &[(20, 300, slow, 2), (100, 100, slow, 2)]);
}

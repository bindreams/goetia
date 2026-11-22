//! The `stop-action:` field: what a stop asks of the daemon, and how long it waits for the answer.
//!
//! ```yaml
//! stop-action: terminate        # short form of:
//! stop-action:
//!   send-signal: terminate      # the default
//!   timeout: 30s                # whole seconds, 1s to 24h; `null` waits as long as the platform allows
//! ```
//!
//! `send-signal` is one of the portable names `terminate`, `interrupt`, `quit` and `kill` (also read as
//! `SIGTERM`, `SIGINT`, `SIGQUIT` and `SIGKILL`, any case, `SIG` optional), a Windows name (`ctrl-c`,
//! `ctrl-break`, `wm-close`) or a native signal of systemd (`sigusr1`, `SIGRTMIN+3`, any case; a typo is
//! an error, never a silent `SIGTERM`). `kill` sends nothing to answer and waits
//! for nothing: its `timeout` is `null`, and any other value is refused.
//!
//! [`RawStopAction`] is what is read: both forms, and each leaf as text, so `${VAR}` can be
//! substituted before [`parse_stop_signal`] and [`parse_stop_timeout`] judge it. [`StopAction`] is what
//! `resolve` makes of it. A `timeout` of `null` is the one explicit `null` a manifest may write
//! (`no_null`'s scan lets it through).

use std::fmt;
use std::time::Duration;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::backend::BackendKind;
use super::resolve::invalid;
use super::{DaemonKind, Id, Protocol};
use crate::error::Error;

// Resolved ============================================================================================================

/// What a stop asks of a daemon, resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopAction {
    pub send_signal: StopSignal,
    /// How long a stop waits for the daemon to exit before the manager ends it. `None` waits as
    /// long as the platform allows. Whole seconds from [`StopAction::MIN_TIMEOUT`] to
    /// [`StopAction::MAX_TIMEOUT`]; always `None` for [`StopSignal::Kill`].
    pub timeout: Option<Duration>,
}

impl StopAction {
    /// The `timeout` a daemon that is asked to stop (any signal but `kill`) has when the manifest
    /// writes none.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
    /// What a stop is given beyond its `timeout` to be torn down and recorded: the wait of `stop` and
    /// `restart` is the `timeout` plus this.
    pub const TEARDOWN_MARGIN: Duration = Duration::from_secs(10);
    pub const MIN_TIMEOUT: Duration = Duration::from_secs(1);
    pub const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

    /// The `timeout` a manifest that writes none gets, which depends on the signal.
    pub fn default_timeout(send_signal: StopSignal) -> Option<Duration> {
        match send_signal {
            StopSignal::Kill => None,
            _ => Some(Self::DEFAULT_TIMEOUT),
        }
    }

    /// The action the leaves a manifest wrote make: a leaf it did not write is its default, and the
    /// default timeout depends on the signal. `resolve` and the judgement of a pass that still holds a
    /// substitution both read the leaves this way.
    pub(crate) fn from_leaves(send_signal: Option<StopSignal>, timeout: Option<Option<Duration>>) -> StopAction {
        let send_signal = send_signal.unwrap_or(StopAction::default().send_signal);
        StopAction {
            send_signal,
            timeout: timeout.unwrap_or_else(|| StopAction::default_timeout(send_signal)),
        }
    }

    /// The longest a stop of this action takes, from the request to the manager's record: the `timeout`
    /// and the [`TEARDOWN_MARGIN`](Self::TEARDOWN_MARGIN); `None` where there is no timeout. A `kill` waits for
    /// nothing, so it is the margin alone. The one rule that the CLI's wait for a stop, `STOP_PENDING`'s wait
    /// hint and a Windows service's pre-shutdown time all read.
    pub fn longest_stop(&self) -> Option<Duration> {
        match self.send_signal {
            StopSignal::Kill => Some(Self::TEARDOWN_MARGIN),
            _ => self.timeout.map(|timeout| timeout + Self::TEARDOWN_MARGIN),
        }
    }

    /// Why this stop action cannot be a daemon's, or `None`: the rules of every field, shared by
    /// `resolve` and by [`ServiceSpec::new`](super::ServiceSpec::new) (which a blob's spec passes
    /// through), so a value no manifest could write is refused wherever it came from.
    pub(crate) fn defect(&self) -> Option<String> {
        if let Some(timeout) = self.timeout {
            if let Some(defect) = timeout_defect(timeout) {
                return Some(defect);
            }
            if self.send_signal == StopSignal::Kill {
                return Some(kill_with_timeout(timeout));
            }
        }
        None
    }
}

impl StopAction {
    /// What `backend` cannot honour of this stop action for a daemon of `kind` and `protocol`, leaf by
    /// leaf: the one judgement, which `resolve` and every generator share. `resolve` judges a manifest's
    /// values once they are settled; a generator judges a spec from a blob, which `resolve` never saw.
    ///
    /// - every backend: a signal it cannot send ([`StopSignal::defect`]);
    /// - launchd: a wait it cannot give ([`launchd_timeout_defect`]);
    /// - Windows: a stop that Task Scheduler (a task) or SCM (a `protocol: native` service) decides
    ///   itself, so a leaf that differs from the default is a request it cannot honour ([`Written`]).
    pub(crate) fn refuse(&self, backend: BackendKind, kind: DaemonKind, protocol: Protocol) -> StopRefusals {
        let mut refusals = StopRefusals {
            signal: self.send_signal.defect(backend),
            timeout: None,
        };
        match backend {
            BackendKind::Systemd => {}
            BackendKind::Launchd => refusals.timeout = launchd_timeout_defect(self.send_signal, self.timeout),
            BackendKind::Scm => {
                let windows = scm_refusals(ScmDaemon::of(kind, protocol), Written::of(self));
                refusals.signal = refusals.signal.or(windows.signal);
                refusals.timeout = windows.timeout;
            }
        }
        refusals
    }
}

/// What a backend refuses of each leaf of a stop action ([`StopAction::refuse`]), as the message that
/// says why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StopRefusals {
    signal: Option<String>,
    timeout: Option<String>,
}

impl StopRefusals {
    /// Only the refusals of the leaves that are in play: a sweep of what an override wrote judges
    /// that, and not a base's value, which the override is not answerable for.
    pub(crate) fn only(self, send_signal: bool, timeout: bool) -> Self {
        StopRefusals {
            signal: self.signal.filter(|_| send_signal),
            timeout: self.timeout.filter(|_| timeout),
        }
    }

    /// The timeout refusal that holds for a signal that is not known as it does for a `kill` (`self`),
    /// and no signal refusal: what holds for every signal the pending one could become, since a refusal
    /// of a timeout depends on the signal only as `kill` or not.
    pub(crate) fn holding_for_every_signal(self, kill: StopRefusals) -> Self {
        StopRefusals {
            signal: None,
            timeout: self.timeout.filter(|_| kill.timeout.is_some()),
        }
    }

    /// The refusal to report: the signal's, else the timeout's.
    pub(crate) fn first(self) -> Option<(StopField, String)> {
        match (self.signal, self.timeout) {
            (Some(message), _) => Some((StopField::SendSignal, message)),
            (None, Some(message)) => Some((StopField::Timeout, message)),
            (None, None) => None,
        }
    }

    /// [`StopRefusals::first`] as the error of `id`'s generator.
    pub(crate) fn into_result(self, id: &Id) -> Result<(), Error> {
        match self.first() {
            Some((_, message)) => Err(invalid(id, &message)),
            None => Ok(()),
        }
    }
}

impl Default for StopAction {
    fn default() -> Self {
        StopAction {
            send_signal: StopSignal::Terminate,
            timeout: Some(StopAction::DEFAULT_TIMEOUT),
        }
    }
}

/// What a stop sends to the daemon to ask it to exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopSignal {
    /// SIGTERM; on Windows, Ctrl+C. The one signal of every platform.
    Terminate,
    /// SIGINT; on Windows, Ctrl+C.
    Interrupt,
    /// SIGQUIT; on Windows, Ctrl+Break.
    Quit,
    /// Ends the daemon at once: no signal to answer, no wait.
    Kill,
    /// A Windows console control event, which systemd and launchd have no equivalent of.
    CtrlC,
    CtrlBreak,
    /// `WM_CLOSE`, posted to the daemon's top-level windows.
    WmClose,
    /// A signal as systemd names it.
    Native(NativeSignal),
}

/// A signal systemd's `KillSignal=` accepts that has no portable name. Read with
/// [`StopSignal`]'s `FromStr`, and written canonically (`SIGUSR1`).
///
/// Small on purpose: a spec holds one, and a spec is what `Installed` carries by value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeSignal(Native);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Native {
    /// `SIGUSR1`: the index of a name in [`NATIVE_SIGNALS`].
    Named(u8),
    /// `SIGRTMIN+n`, `n` at most [`MAX_RT_OFFSET`].
    RtMin(u8),
    /// `SIGRTMAX-n`, `n` at most [`MAX_RT_OFFSET`].
    RtMax(u8),
}

/// systemd's names for the Linux signals (`signal_to_string`) that have no portable name, each with its `SIG`
/// prefix. `SIGTERM`, `SIGINT`, `SIGQUIT` and `SIGKILL` are the portable signals ([`portable_by_native_name`]).
const NATIVE_SIGNALS: [&str; 27] = [
    "SIGHUP",
    "SIGILL",
    "SIGTRAP",
    "SIGABRT",
    "SIGBUS",
    "SIGFPE",
    "SIGUSR1",
    "SIGSEGV",
    "SIGUSR2",
    "SIGPIPE",
    "SIGALRM",
    "SIGSTKFLT",
    "SIGCHLD",
    "SIGCONT",
    "SIGSTOP",
    "SIGTSTP",
    "SIGTTIN",
    "SIGTTOU",
    "SIGURG",
    "SIGXCPU",
    "SIGXFSZ",
    "SIGVTALRM",
    "SIGPROF",
    "SIGWINCH",
    "SIGIO",
    "SIGPWR",
    "SIGSYS",
];

/// systemd's `signal_from_string` takes `RTMIN+n` and `RTMAX-n` for `0 <= n <= SIGRTMAX - SIGRTMIN`
/// (its comment, "RTMIN+n (0 <= n <= SIGRTMAX-SIGRTMIN)", is the same in v242, v255 and main), which is
/// 30 on glibc (`SIGRTMIN` 34, `SIGRTMAX` 64). A libc with another `SIGRTMIN` has another range, which
/// is systemd's to refuse at install.
const MAX_RT_OFFSET: u8 = 30;

impl fmt::Display for NativeSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Native::Named(index) => f.write_str(NATIVE_SIGNALS[usize::from(index)]),
            Native::RtMin(0) => f.write_str("SIGRTMIN"),
            Native::RtMin(n) => write!(f, "SIGRTMIN+{n}"),
            Native::RtMax(0) => f.write_str("SIGRTMAX"),
            Native::RtMax(n) => write!(f, "SIGRTMAX-{n}"),
        }
    }
}

impl fmt::Display for StopSignal {
    /// The manifest's spelling, which `FromStr` reads back.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StopSignal::Terminate => f.write_str("terminate"),
            StopSignal::Interrupt => f.write_str("interrupt"),
            StopSignal::Quit => f.write_str("quit"),
            StopSignal::Kill => f.write_str("kill"),
            StopSignal::CtrlC => f.write_str("ctrl-c"),
            StopSignal::CtrlBreak => f.write_str("ctrl-break"),
            StopSignal::WmClose => f.write_str("wm-close"),
            StopSignal::Native(signal) => signal.fmt(f),
        }
    }
}

impl std::str::FromStr for StopSignal {
    type Err = String;

    /// Read the manifest's spelling: a portable or Windows name (lowercase), the portable signals by
    /// their native names (`SIGTERM`, `SIGINT`, `SIGQUIT`, `SIGKILL`: any case, `SIG` optional), or a
    /// native signal of systemd (any case, `SIG` prefix required). The error says what was expected.
    fn from_str(text: &str) -> Result<StopSignal, String> {
        Ok(match text {
            "terminate" => StopSignal::Terminate,
            "interrupt" => StopSignal::Interrupt,
            "quit" => StopSignal::Quit,
            "kill" => StopSignal::Kill,
            "ctrl-c" => StopSignal::CtrlC,
            "ctrl-break" => StopSignal::CtrlBreak,
            "wm-close" => StopSignal::WmClose,
            other => match portable_by_native_name(other) {
                Some(signal) => signal,
                None => StopSignal::Native(NativeSignal::parse(other).ok_or_else(|| {
                    format!(
                        "`send-signal` is `{other}`; expected `terminate`, `interrupt`, `quit`, `kill`, `ctrl-c`, \
                        `ctrl-break`, `wm-close`, or a signal systemd knows (`sigusr1`, `SIGRTMIN+3`)"
                    )
                })?),
            },
        })
    }
}

/// The portable signal a native name (`SIGTERM`, `term`, `Sigint`) stands for: the same signal as its
/// portable name, so every rule for that one applies.
fn portable_by_native_name(text: &str) -> Option<StopSignal> {
    let upper = text.to_ascii_uppercase();
    match upper.strip_prefix("SIG").unwrap_or(&upper) {
        "TERM" => Some(StopSignal::Terminate),
        "INT" => Some(StopSignal::Interrupt),
        "QUIT" => Some(StopSignal::Quit),
        "KILL" => Some(StopSignal::Kill),
        _ => None,
    }
}

impl StopSignal {
    /// Why `backend` cannot send this signal, or `None`. Where a platform cannot, the value is
    /// refused rather than turned into another signal.
    pub(crate) fn defect(self, backend: BackendKind) -> Option<String> {
        let (accepts, only) = match backend {
            BackendKind::Systemd => (
                !matches!(self, StopSignal::CtrlC | StopSignal::CtrlBreak | StopSignal::WmClose),
                "systemd does not have it",
            ),
            BackendKind::Launchd => (
                matches!(self, StopSignal::Terminate | StopSignal::Kill),
                "launchd sends only SIGTERM (and SIGKILL after the timeout)",
            ),
            BackendKind::Scm => (
                !matches!(self, StopSignal::Native(_)),
                "Windows has no native signal; it has `ctrl-c`, `ctrl-break` and `wm-close`",
            ),
        };
        (!accepts).then(|| format!("`send-signal: {self}` is refused on {backend}: {only}"))
    }
}

impl NativeSignal {
    fn parse(text: &str) -> Option<NativeSignal> {
        let upper = text.to_ascii_uppercase();
        let name = upper.strip_prefix("SIG")?;
        if let Some(offset) = name.strip_prefix("RTMIN") {
            return rt_offset(offset, '+', MAX_RT_OFFSET).map(|n| NativeSignal(Native::RtMin(n)));
        }
        if let Some(offset) = name.strip_prefix("RTMAX") {
            return rt_offset(offset, '-', MAX_RT_OFFSET).map(|n| NativeSignal(Native::RtMax(n)));
        }
        let index = NATIVE_SIGNALS
            .iter()
            .position(|known| known.strip_prefix("SIG") == Some(name))?;
        Some(NativeSignal(Native::Named(u8::try_from(index).expect("a table of 27"))))
    }
}

/// The `n` of `+n` or `-n` (`sign`), or `0` for no suffix at all.
fn rt_offset(rest: &str, sign: char, max: u8) -> Option<u8> {
    if rest.is_empty() {
        return Some(0);
    }
    let digits = rest.strip_prefix(sign)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u8>().ok().filter(|n| *n <= max)
}

// Judging a daemon ====================================================================================================

/// The longest wait launchd honours: macOS 26.4 and later cut any `ExitTimeOut` to this (measured;
/// Apple does not document it).
pub(crate) const LAUNCHD_MAX_TIMEOUT: Duration = Duration::from_secs(60);

/// Why launchd cannot wait `timeout` for a daemon that is asked to stop with `signal`, or `None`: a
/// wait over [`LAUNCHD_MAX_TIMEOUT`], and no timeout (a wait with no limit) for any signal but `kill`,
/// which waits for nothing. Refused rather than cut, on every macOS.
fn launchd_timeout_defect(signal: StopSignal, timeout: Option<Duration>) -> Option<String> {
    let cap = "launchd cuts any wait to 60 s (macOS 26.4 and later, measured)";
    match timeout {
        Some(timeout) if timeout > LAUNCHD_MAX_TIMEOUT => Some(format!(
            "`stop-action.timeout` {timeout:?} is refused on launchd: {cap}; write 60s or less"
        )),
        None if signal != StopSignal::Kill => Some(format!(
            "`stop-action.timeout: null` is refused on launchd: {cap}, so it has no wait without a limit; \
             write 60s or less"
        )),
        _ => None,
    }
}

/// What Windows cannot honour of a `stop-action` that `written` says differs from the default. A task
/// is stopped by Task Scheduler, and a `protocol: native` service by SCM's own stop: in neither does
/// goetia send a signal, so a `send-signal` is refused; Task Scheduler also bounds the wait itself, so
/// a task refuses a `timeout` too.
fn scm_refusals(daemon: ScmDaemon, written: Written) -> StopRefusals {
    let refusal = |field: StopField, reason: &str| format!("`stop-action.{}`: {reason}", field.leaf());
    let signal = match daemon {
        ScmDaemon::Task if written.send_signal => Some(refusal(
            StopField::SendSignal,
            "a task on Windows is stopped by Task Scheduler, which decides the signal; set `stop-action` \
            under `backend-specific` for systemd and launchd instead",
        )),
        ScmDaemon::NativeService if written.send_signal => Some(refusal(
            StopField::SendSignal,
            "a `protocol: native` service on Windows is stopped by SCM's own stop, which the service \
            handles itself, so there is no signal to choose; `timeout` still applies",
        )),
        _ => None,
    };
    let timeout = match daemon {
        ScmDaemon::Task if written.timeout => Some(refusal(
            StopField::Timeout,
            "a task on Windows is stopped by Task Scheduler, which decides how long it waits; set \
            `stop-action` under `backend-specific` for systemd and launchd instead",
        )),
        _ => None,
    };
    StopRefusals { signal, timeout }
}

/// A leaf of `stop-action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopField {
    SendSignal,
    Timeout,
}

impl StopField {
    fn leaf(self) -> &'static str {
        match self {
            StopField::SendSignal => "send-signal",
            StopField::Timeout => "timeout",
        }
    }
}

/// The kinds of Windows daemon [`scm_refusals`] tells apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScmDaemon {
    Task,
    NativeService,
    PlainService,
}

impl ScmDaemon {
    fn of(kind: DaemonKind, protocol: Protocol) -> Self {
        match (kind, protocol) {
            (DaemonKind::Task, _) => ScmDaemon::Task,
            (DaemonKind::Service, Protocol::Plain) => ScmDaemon::PlainService,
            (DaemonKind::Service, Protocol::Native) => ScmDaemon::NativeService,
        }
    }
}

/// Which of a `stop-action`'s fields something wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Written {
    pub send_signal: bool,
    pub timeout: bool,
}

impl Written {
    /// What a resolved `action` shows of the manifest: a leaf that is the default may or may not have
    /// been written, and counts as not.
    pub(crate) fn of(action: &StopAction) -> Self {
        let default = StopAction::default();
        Written {
            send_signal: action.send_signal != default.send_signal,
            timeout: action.timeout != StopAction::default_timeout(action.send_signal),
        }
    }
}

// Parsing the leaves ==================================================================================================

/// An authored `send-signal:` into a [`StopSignal`]. `pub(crate)`: see `resolve::parse_restart`.
pub(crate) fn parse_stop_signal(id: &Id, raw: &str) -> Result<StopSignal, Error> {
    raw.parse::<StopSignal>().map_err(|message| invalid(id, &message))
}

/// An authored `timeout:` into a [`Duration`] of whole seconds from `1s` to `24h`.
pub(crate) fn parse_stop_timeout(id: &Id, raw: &str) -> Result<Duration, Error> {
    let timeout = humantime::parse_duration(raw).map_err(|source| {
        super::resolve::invalid_from(
            id,
            &format!("field `stop-action.timeout` is `{raw}`, which is not a duration (e.g. `30s`, `2m`) or `null`"),
            source,
        )
    })?;
    match timeout_defect(timeout) {
        Some(defect) => Err(invalid(id, &defect)),
        None => Ok(timeout),
    }
}

fn timeout_defect(timeout: Duration) -> Option<String> {
    if timeout.subsec_nanos() != 0 {
        return Some(format!(
            "`stop-action.timeout` {timeout:?} is not a whole number of seconds; write whole seconds, or `null` \
             to wait as long as the platform allows"
        ));
    }
    if timeout < StopAction::MIN_TIMEOUT || timeout > StopAction::MAX_TIMEOUT {
        return Some(format!(
            "`stop-action.timeout` {timeout:?} is outside 1s to 24h; write a value in that range, or `null` to \
             wait as long as the platform allows"
        ));
    }
    None
}

/// The refusal of a `timeout` beside `send-signal: kill`: it ends the daemon at once, so a wait
/// means nothing.
pub(crate) fn kill_with_timeout(timeout: Duration) -> String {
    format!(
        "`stop-action.timeout` {timeout:?} cannot go with `send-signal: kill`, which ends the daemon at once; \
         omit `timeout`, or write `null`"
    )
}

// Raw =================================================================================================================

/// `stop-action:` as authored, in either form; a leaf is `None` where the manifest wrote none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct RawStopAction {
    pub send_signal: Option<String>,
    pub timeout: Option<RawTimeout>,
    /// The `timeout` is the base's, kept under an override that wrote its own `send-signal`. If that
    /// signal is `kill`, which has no timeout of its own, it is dropped rather than refused.
    pub timeout_from_base: bool,
    /// The signal of the base that the override's replaced. A base `kill`'s `timeout: null` is that signal's
    /// own, and the override's other signal does not take it; and a base signal that is not one is refused
    /// though nothing else uses it.
    pub base_signal: Option<BaseSignal>,
}

/// A base `send-signal:` that an override's replaced, as far as substitution has taken it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BaseSignal {
    /// As authored: it may still hold `${...}` references.
    Written(String),
    /// Substituted; `from_env` says whether `.env` supplied any of it.
    Substituted { text: String, from_env: bool },
    /// Could not be substituted (an unset variable): not known to be anything, `kill` included.
    Unsubstituted,
}

/// `timeout:` as authored: the one field where `null` is a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RawTimeout {
    Null,
    Text(String),
}

impl RawStopAction {
    /// `self` laid over `base`, leaf by leaf: what a backend's override writes wins, and what it
    /// does not write stays the base's, except that a base's `timeout` goes with `send-signal: kill`
    /// only if the override writes one too, and a base `kill`'s `null` goes with no other signal (see
    /// [`RawStopAction::timeout_from_base`]).
    pub(super) fn over(&self, base: &Option<RawStopAction>) -> RawStopAction {
        let base = base.clone().unwrap_or_default();
        let timeout_from_base = self.send_signal.is_some() && self.timeout.is_none() && base.timeout.is_some();
        RawStopAction {
            send_signal: self.send_signal.clone().or(base.send_signal.clone()),
            timeout_from_base,
            base_signal: self.send_signal.as_ref().and(base.send_signal).map(BaseSignal::Written),
            timeout: self.timeout.clone().or(base.timeout),
        }
    }
}

/// `RawStopAction`'s serde shape. Hand-written, not `untagged`, so an error names the field.
struct StopActionWire(RawStopAction);

impl<'de> Deserialize<'de> for StopActionWire {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StopActionVisitor).map(StopActionWire)
    }
}

/// The `deserialize_with` for an `Option<RawStopAction>` field: absent and `null` are `None`, as for
/// any `Option` (`no_null` then refuses the `null`).
pub(super) fn deserialize_stop_action<'de, D>(d: D) -> Result<Option<RawStopAction>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<StopActionWire>::deserialize(d).map(|action| action.map(|StopActionWire(action)| action))
}

struct StopActionVisitor;

impl<'de> Visitor<'de> for StopActionVisitor {
    type Value = RawStopAction;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a signal name, or a mapping of `send-signal` and `timeout`")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<RawStopAction, E> {
        Ok(RawStopAction {
            send_signal: Some(v.to_owned()),
            ..RawStopAction::default()
        })
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<RawStopAction, A::Error> {
        let mut action = RawStopAction::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "send-signal" => {
                    if action.send_signal.is_some() {
                        return Err(de::Error::duplicate_field("send-signal"));
                    }
                    action.send_signal = Some(map.next_value()?);
                }
                "timeout" => {
                    if action.timeout.is_some() {
                        return Err(de::Error::duplicate_field("timeout"));
                    }
                    action.timeout = Some(match map.next_value::<Option<String>>()? {
                        Some(text) => RawTimeout::Text(text),
                        None => RawTimeout::Null,
                    });
                }
                other => return Err(de::Error::unknown_field(other, &["send-signal", "timeout"])),
            }
        }
        Ok(action)
    }
}

#[cfg(test)]
#[path = "stop_action_tests.rs"]
mod stop_action_tests;

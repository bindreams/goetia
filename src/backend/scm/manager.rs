//! The effectful SCM backend (`#[cfg(windows)]`).
//!
//! Handles both `Kind::Simple` (via `goetia-shim`, `src/bin/shim/`) and
//! `Kind::Managed`; `generate::registration` builds the `ImagePath`/blob
//! for both. `Kind::Simple`'s own integration tests live in
//! `tests/shim_integration.rs`, not `tests/scm_integration.rs`, because they
//! need `CARGO_BIN_EXE_goetia-shim`.
//!
//! ## The five traps this module addresses
//!
//! **1. Ownership has only one proof for `type: managed`.** `type: simple`'s
//! `ImagePath` (`goetia-shim.exe <id>`, written atomically by `CreateServiceW`)
//! is a second proof of ownership independent of `Parameters`; `type: managed`'s
//! `ImagePath` is just the daemon's own command, indistinguishable from any
//! other service that happens to run the same executable, so it cannot serve
//! that role. This means the crash window between `CreateServiceW` and the
//! `Parameters` write (see point 5) has no in-tool escape route for
//! `type: managed` the way it does for `type: simple`: `discover` classifies
//! such an orphan as [`Ownership::Foreign`] (no marker at all), and the next
//! `install` refuses with [`decide::foreign_recovery`]'s named command — the
//! real Windows analogue of `sc.exe delete <id>` (or Services MMC), which is
//! in fact the exact remedy: nothing but deleting the orphan and re-running
//! `install` can recover it. `interrupted_install_is_recoverable` in
//! `tests/scm_integration.rs` pins this. `uninstall`, meanwhile, still does
//! not need a *decodable* blob — only the `Marker` field, so a corrupted
//! (but present) `Spec` value never wedges an id; see [`require_ours`].
//!
//! **2. Stopping (and starting) must not poll.** `windows-service`'s
//! `stop()`/`start()` return as soon as SCM accepts the request, not once
//! the transition completes, and the crate wraps no wait primitive. See
//! `scm_wait`, a port of `~/src/hole/crates/bridge/src/cutover/scm_wait.rs`
//! using `NotifyServiceStatusChangeW` — a real kernel rendezvous, never a
//! `Sleep`+`QueryServiceStatusEx` poll.
//!
//! **3. Uninstall must confirm a real stop before deleting.** `DeleteService`
//! on a running service only *marks* it for deletion — the registry key
//! survives until every `SC_HANDLE` closes and the service actually stops —
//! so the next `install` would meet `ERROR_SERVICE_MARKED_FOR_DELETE`.
//! [`uninstall_locked`] waits for a confirmed `STOPPED` (via `scm_wait`)
//! before opening a *fresh* handle for `DeleteService`, so the handle used
//! to wait is always closed first.
//!
//! **4. A real account needs `SeServiceLogonRight`.** `CreateServiceW`
//! succeeds without it; the first start then fails with error 1069
//! (`ERROR_LOGON_FAILURE`) while `install` has already reported success. See
//! `identity::grant_service_logon_right`, called for every non-`LocalSystem`
//! account on every `install`.
//!
//! **5. Create with `SERVICE_DEMAND_START`, write `Parameters` immediately
//! after.** Not `SERVICE_DISABLED` (which would block an explicit `start`),
//! and no separate start-type flip — boot-enablement is outside every drift
//! comparison, so an install interrupted before the flip is not a
//! meaningful failure mode here (see point 1 for the window that *is*).
//! `enable`/`disable` flip between `SERVICE_AUTO_START` and
//! `SERVICE_DEMAND_START` after the fact, via `ChangeServiceConfigW` with
//! `SERVICE_NO_CHANGE` on every other field (see [`set_start_type`]).
//!
//! ## `env` on `type: managed` (spec §8 must-verify #5)
//!
//! Per-service environment variables **are** supported for `type: managed`,
//! via the documented (if obscure) `Services\<name>\Environment`
//! `REG_MULTI_SZ` value — a list of `NAME=value` lines SCM adds to the
//! environment of the service process at launch. This is not part of
//! `ScmRegistration`/`render()` (§8's answer arrived after that module was
//! reviewed and hardened as pure, host-independent code — see `generate.rs`'s
//! own doc comment), so `env` sits outside the drift-compared artifact
//! surface: a hand-edit to `Environment` alone is not detected as a
//! conflict, and `env` is unconditionally rewritten to match `new_spec` on
//! every `Create`/`Update`/`Stale`. `managed_kind_environment_availability`
//! in `tests/scm_integration.rs` verifies the mechanism empirically, on the
//! real (elevated) Windows CI runner. This amends the design spec's §2
//! mapping table, which had marked this cell "must verify (#5)".
//!
//! ## Passwords
//!
//! Per the design spec's "Windows accounts" section, a real (non-built-in,
//! non-virtual) account needs a password, supplied via the
//! `GOETIA_SERVICE_PASSWORD` environment variable — never `goetia.yaml`. See
//! `identity::service_password`.

mod identity;
mod registry;
mod scm_wait;

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::PathBuf;

use windows_service::service::{
    Service, ServiceAccess, ServiceActionType, ServiceErrorControl, ServiceFailureResetPeriod, ServiceInfo,
    ServiceStartType, ServiceState as WinState, ServiceType,
};
use windows_service::service_manager::{ServiceManager as WinServiceManager, ServiceManagerAccess};
use windows_sys::Win32::Foundation::{ERROR_SERVICE_DOES_NOT_EXIST, LocalFree};
use windows_sys::Win32::System::Services::{ChangeServiceConfigW, SERVICE_NO_CHANGE};
use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

use crate::backend::scm::generate::{self, FailureActions as GenFailureActions, ScmRegistration};
use crate::blob::Blob;
use crate::decide::{self, Outcome, Ownership};
use crate::error::{Error, Result};
use crate::manager::{Installed, ServiceManager, State, Status};
use crate::spec::{DaemonSpec, Id, Kind, User, Warning};

// ScmManager ==========================================================================================================

/// The Windows `ServiceManager` implementation. Holds no state of its own —
/// every operation opens exactly the SCM/registry handles it needs and
/// closes them again (see trap 3 above).
#[derive(Debug, Default, Clone, Copy)]
pub struct ScmManager;

impl ScmManager {
    pub fn new() -> Self {
        Self
    }
}

impl ServiceManager for ScmManager {
    fn install(&self, spec: &DaemonSpec, force: bool) -> Result<Outcome> {
        let d = discover(spec)?;
        let outcome = decide::decide(
            &d.found,
            d.on_disk.as_deref(),
            &d.desired,
            spec,
            crate::version(),
            force,
            // SCM has no drop-in mechanism: everything Goetia writes lives
            // under the service's own key. See `env_is_outside_the_compared
            // _surface` in the module docs for the one value that is not yet
            // in `render()`'s surface.
            false,
        );
        match outcome {
            Outcome::Create | Outcome::Update { .. } | Outcome::Stale { .. } => {
                apply(spec, &d.reg, matches!(d.found, Ownership::Absent), d.current_start_type)?;
            }
            // See `reapply_uncompared_effects`'s own doc comment.
            Outcome::UpToDate => reapply_uncompared_effects(spec, &d.reg)?,
            _ => {}
        }
        Ok(outcome)
    }

    fn preview_install(&self, spec: &DaemonSpec) -> Result<Outcome> {
        let d = discover(spec)?;
        Ok(decide::decide(
            &d.found,
            d.on_disk.as_deref(),
            &d.desired,
            spec,
            crate::version(),
            false,
            // SCM has no drop-in mechanism; see the `install` call site.
            false,
        ))
    }

    fn uninstall(&self, id: &Id) -> Result<()> {
        let (scm, service) = open_existing(id, ServiceAccess::QUERY_STATUS)?;
        require_ours(id)?;
        // Close this handle before `uninstall_locked` opens (and eventually
        // deletes through) its own — trap 3 is specifically that a
        // still-open handle, of *any* access, defers `DeleteService`'s
        // actual removal until every handle to the service has closed, not
        // only the one `DeleteService` itself was called on.
        drop(service);
        uninstall_locked(&scm, id)
    }

    fn enable(&self, id: &Id) -> Result<()> {
        let (_scm, service) = open_existing(id, ServiceAccess::CHANGE_CONFIG)?;
        require_ours(id)?;
        set_start_type(&service, ServiceStartType::AutoStart).map_err(|e| Error::Other(format!("enable `{id}`: {e}")))
    }

    fn disable(&self, id: &Id) -> Result<()> {
        let (_scm, service) = open_existing(id, ServiceAccess::CHANGE_CONFIG)?;
        require_ours(id)?;
        set_start_type(&service, ServiceStartType::OnDemand).map_err(|e| Error::Other(format!("disable `{id}`: {e}")))
    }

    fn start(&self, id: &Id) -> Result<()> {
        let (_scm, service) = open_existing(id, ServiceAccess::QUERY_STATUS)?;
        require_ours(id)?;
        drop(service);
        let mut actor = scm_wait::SystemScmActor::open(id.as_str())
            .map_err(|e| Error::Other(format!("open `{id}` to start it: {e}")))?;
        scm_wait::start_via_notify(&mut actor).map_err(|e| Error::Other(format!("start `{id}`: {e}")))
    }

    fn stop(&self, id: &Id) -> Result<()> {
        let (_scm, service) = open_existing(id, ServiceAccess::QUERY_STATUS)?;
        require_ours(id)?;
        drop(service);
        let mut actor = scm_wait::SystemScmActor::open(id.as_str())
            .map_err(|e| Error::Other(format!("open `{id}` to stop it: {e}")))?;
        scm_wait::stop_via_notify(&mut actor).map_err(|e| Error::Other(format!("stop `{id}`: {e}")))
    }

    fn status(&self, id: &Id) -> Result<Status> {
        let (_scm, service) = open_existing(id, ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG)?;
        // Stricter than `require_ours`: an undecodable blob (or one whose
        // account can no longer be resolved — see `classify`) must not
        // fabricate a plausible-looking `Status` — see the trait doc
        // comment on `ServiceManager::status`.
        let params = registry::read_parameters(id.as_str())?;
        let blob = generate::extract(&params)?.ok_or_else(|| foreign(id))?;
        identity::resolve(&blob.spec.user)
            .map_err(|e| Error::Other(format!("`{id}`'s account could not be resolved: {e}")))?;

        let st = service
            .query_status()
            .map_err(|e| to_error(&format!("query status for `{id}`"), e))?;
        let cfg = service
            .query_config()
            .map_err(|e| to_error(&format!("query configuration for `{id}`"), e))?;
        Ok(Status {
            state: map_state(st.current_state),
            pid: st.process_id,
            enabled: cfg.start_type == ServiceStartType::AutoStart,
        })
    }

    fn list(&self) -> Result<Vec<Installed>> {
        let mut out = Vec::new();
        let mut unreadable = 0usize;
        for name in registry::list_service_names()? {
            // A registry read failure for one unrelated service (e.g. a
            // driver whose `Parameters` key carries a restrictive ACL) must
            // not take `list` down for every other daemon — the same
            // per-entry fault tolerance `Installed::OursUnreadable` exists
            // for on the decode side. Since a read failure here means we
            // cannot even tell whether `name` carries Goetia's marker, this
            // is reported and skipped rather than guessed at either way.
            let params = match registry::read_parameters(&name) {
                Ok(p) => p,
                Err(e) => {
                    // Most services on a Windows box carry an ACL that denies
                    // a non-elevated read of their `Parameters`, so warning per
                    // service turns `goetia daemon list` into a wall of noise
                    // about services that were never ours. But staying silent
                    // would be a lie in the other direction: a denied read
                    // means ownership is *unknown*, so one of ours could be
                    // missing from the listing. Count them and say so once.
                    unreadable += 1;
                    let _ = e;
                    continue;
                }
            };
            let blob = match generate::extract(&params) {
                Ok(None) => continue,
                Ok(Some(blob)) => blob,
                Err(e) => {
                    out.push(Installed::OursUnreadable {
                        name,
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
            // A blob can decode perfectly and still name an account (e.g. a
            // `user.id` SID) that no longer exists on this host — see
            // `classify`'s identical check on the `install`/`diff` side.
            // `list`/`status` must agree with `install` about which ids are
            // "genuinely ours and readable", or a daemon `list` reports
            // healthy could refuse the very next `install`.
            if let Err(e) = identity::resolve(&blob.spec.user) {
                out.push(Installed::OursUnreadable {
                    name,
                    reason: format!("marked ours, but its account could not be resolved: {e}"),
                });
                continue;
            }
            match query_live(&name) {
                Ok((state, enabled)) => out.push(Installed::Ours {
                    spec: blob.spec,
                    state,
                    enabled,
                }),
                Err(e) => out.push(Installed::OursUnreadable {
                    name,
                    reason: format!("marked ours, but its live status could not be read: {e}"),
                }),
            }
        }
        if unreadable > 0 {
            eprintln!("warning: {}", unreadable_notice(unreadable));
        }
        Ok(out)
    }
}

// shim support ========================================================================================================

/// Read and decode the metadata blob for `id` directly from
/// `Services\<id>\Parameters`. `goetia-shim` (`src/bin/shim/`, a separate
/// `[[bin]]` target and therefore a separate crate that only ever sees this
/// module's `pub` surface) calls this: it knows only its service id
/// (`argv[1]`), not a `DaemonSpec` to hand to [`discover`], and must not
/// pull in `ServiceManager`'s effectful install path just to read one
/// value. `registry` itself stays private — this is the one read the shim
/// needs, not the whole module.
///
/// `Ok(None)` means no `Marker` at all: unreachable in practice (SCM only
/// ever launches the shim against a service *this backend itself* created
/// with `ImagePath` naming the shim, and `apply` always writes `Parameters`
/// immediately after), but returned rather than folded into `Err` so a
/// caller can still tell "no marker" apart from "a marker that fails to
/// decode" — the shim's own version-skew handling (see
/// `src/bin/shim/logging.rs`) needs exactly that distinction to phrase its
/// failure message.
pub fn read_spec_blob(id: &str) -> Result<Option<Blob>> {
    let params = registry::read_parameters(id)?;
    generate::extract(&params)
}

// discover ============================================================================================================

/// What `install`/`preview_install` need from a fresh read of the world:
/// the registration this run would write, its canonical text, and how the
/// existing service (if any) is classified.
struct Discovery {
    reg: ScmRegistration,
    desired: String,
    found: Ownership,
    on_disk: Option<String>,
    /// `Some(dwStartType)` when the service already exists — threaded into
    /// `apply`'s update path so a routine spec-driven `ChangeServiceConfigW`
    /// preserves whatever `enable`/`disable` last set, rather than
    /// resetting it. `None` when absent (a fresh `Create` always uses
    /// `SERVICE_DEMAND_START`; see trap 5).
    current_start_type: Option<ServiceStartType>,
}

/// The shim's expected location: a sibling of the currently running
/// `goetia` binary. Only consulted for `Kind::Simple` — `generate::registration`
/// ignores it entirely for `Kind::Managed`.
///
/// `GOETIA_SHIM_PATH` overrides this, read fresh on every call rather than
/// cached. Production `goetia.exe` never sets it, so it always falls
/// through to the sibling-of-`current_exe` heuristic below — appropriate
/// for the `cargo install`-style layout the design's "Windows shim path"
/// open item describes, where both binaries land in the same directory.
/// `tests/shim_integration.rs`'s own `main` sets it once, before any test
/// runs — see that file's doc comment for why a plain `current_exe()`-based
/// guess cannot find the shim binary from inside a test crate.
fn shim_path() -> PathBuf {
    if let Some(overridden) = std::env::var_os("GOETIA_SHIM_PATH") {
        return PathBuf::from(overridden);
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|dir| dir.join("goetia-shim.exe")))
        .unwrap_or_else(|| PathBuf::from("goetia-shim.exe"))
}

fn print_warnings(warnings: &[Warning]) {
    for w in warnings {
        eprintln!("warning: {}: {}", w.id, w.message);
    }
}

fn discover(spec: &DaemonSpec) -> Result<Discovery> {
    let identity = identity::resolve(&spec.user)?;
    let (reg, warnings) = generate::registration(spec, &identity, &shim_path());
    print_warnings(&warnings);
    let desired = generate::render(&reg);

    let scm = open_scm(ServiceManagerAccess::CONNECT)?;
    let existing = scm.open_service(spec.id.as_str(), ServiceAccess::QUERY_CONFIG);
    let (found, on_disk, current_start_type) = match existing {
        Err(e) if is_not_found(&e) => (Ownership::Absent, None, None),
        Err(e) => return Err(to_error(&format!("open service `{}` for discovery", spec.id), e)),
        Ok(service) => {
            let params = registry::read_parameters(spec.id.as_str())?;
            let found = classify(&params);
            let (live, start_type) = read_live_registration(&service, spec.id.as_str(), &params)?;
            (found, Some(generate::render(&live)), Some(start_type))
        }
    };

    Ok(Discovery {
        reg,
        desired,
        found,
        on_disk,
        current_start_type,
    })
}

/// Classify what's under `Services\<id>\Parameters`, exactly as `discover`
/// needs it for [`decide::decide`]. A blob that decodes but whose account
/// can no longer be resolved (e.g. `user.id` names a SID for an account
/// since deleted) is [`Ownership::OursUnreadable`]: `decide` cannot compute
/// `regenerated` without a working `Identity`, so this backend genuinely
/// cannot regenerate that spec on this host — the same situation a future
/// schema or bit-rotted blob puts it in.
fn classify(params: &BTreeMap<String, String>) -> Ownership {
    match generate::extract(params) {
        Ok(None) => Ownership::Foreign,
        Ok(Some(blob)) => match identity::resolve(&blob.spec.user) {
            Ok(blob_identity) => {
                let (blob_reg, _warnings) = generate::registration(&blob.spec, &blob_identity, &shim_path());
                Ownership::Ours {
                    regenerated: generate::render(&blob_reg),
                    blob,
                }
            }
            Err(e) => Ownership::OursUnreadable {
                reason: format!("cannot resolve the account the embedded spec was installed for: {e}"),
            },
        },
        Err(e) => Ownership::OursUnreadable { reason: e.to_string() },
    }
}

/// Read back everything `render()` needs from the service actually
/// installed at `name`, for the `on_disk` side of `decide`'s comparison —
/// SCM's "artifact" for diffing, since there is no file (see the
/// crate-level design notes on this).
fn read_live_registration(
    service: &Service,
    name: &str,
    live_params: &BTreeMap<String, String>,
) -> Result<(ScmRegistration, ServiceStartType)> {
    let cfg = service
        .query_config()
        .map_err(|e| to_error(&format!("query configuration for `{name}`"), e))?;
    let cmdline = cfg.executable_path.to_string_lossy().into_owned();
    let (executable, arguments) = split_command_line(&cmdline);
    let account = cfg
        .account_name
        .map(|a| a.to_string_lossy().into_owned())
        // `ChangeServiceConfigW`'s NULL-means-"unchanged" quirk (see `apply`'s
        // doc comment) forces a live `LocalSystem` account to be written back
        // as the literal string "LocalSystem", not `None` — normalize it back
        // to `None` here so it renders identically to `generate::registration`'s
        // own `User::Root` mapping and a `LocalSystem` service reads as
        // up-to-date rather than a permanent phantom diff.
        .filter(|a| !a.eq_ignore_ascii_case("LocalSystem"));
    let failure_actions = read_failure_actions(service, name)?;

    Ok((
        ScmRegistration {
            name: name.to_string(),
            display_name: cfg.display_name.to_string_lossy().into_owned(),
            executable,
            arguments,
            account,
            failure_actions,
            parameters: live_params.clone(),
        },
        cfg.start_type,
    ))
}

fn read_failure_actions(service: &Service, name: &str) -> Result<Option<GenFailureActions>> {
    let on_non_crash_failures = service
        .get_failure_actions_on_non_crash_failures()
        .map_err(|e| to_error(&format!("query failure-actions flag for `{name}`"), e))?;
    let raw = service
        .get_failure_actions()
        .map_err(|e| to_error(&format!("query failure actions for `{name}`"), e))?;

    let restart_delay = raw.actions.iter().flatten().find_map(|a| match a.action_type {
        ServiceActionType::Restart => Some(a.delay),
        _ => None,
    });

    Ok(restart_delay.map(|delay| GenFailureActions {
        delay,
        reset_period: match raw.reset_period {
            ServiceFailureResetPeriod::After(d) => d,
            // `GenFailureActions::reset_period` has no "never resets"
            // variant (Goetia itself never writes one — see
            // `generate::FAILURE_RESET_PERIOD`), so a hand-edited `Never`
            // reset period is represented as a value nothing Goetia writes
            // would ever produce, which is exactly what a diff against it
            // needs: visibly different, never accidentally "up to date".
            ServiceFailureResetPeriod::Never => std::time::Duration::from_secs(u32::MAX as u64),
        },
        on_non_crash_failures,
    }))
}

/// `windows-service`'s `ServiceConfig::executable_path` is the raw
/// `lpBinaryPathName` string, unparsed — SCM stores `executable`+`arguments`
/// pre-joined into one command line (see `generate.rs`'s own doc comment on
/// why `ScmRegistration` keeps them separate). Splits it with the real
/// `CommandLineToArgvW`, the same authority `generate_tests.rs`'s
/// `windows_style_split` is checked against.
fn split_command_line(cmdline: &str) -> (PathBuf, Vec<String>) {
    let wide: Vec<u16> = OsStr::new(cmdline).encode_wide().chain(std::iter::once(0)).collect();
    let mut argc: i32 = 0;
    // SAFETY: `wide` is a valid null-terminated UTF-16 string that outlives
    // this call; `argc` is a valid, aligned `i32` out-param.
    let argv = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut argc) };
    if argv.is_null() {
        // Only reachable for a malformed command line SCM itself would never
        // have accepted — degrade to "no arguments" rather than panic in
        // discovery, which callers (`list`/`status`) need to keep working
        // for every other installed service.
        return (PathBuf::new(), Vec::new());
    }

    let mut result = Vec::with_capacity(argc.max(0) as usize);
    for idx in 0..argc as isize {
        // SAFETY: `argv` points to `argc` valid pointers to null-terminated
        // UTF-16 strings, per the documented `CommandLineToArgvW` contract.
        let ptr = unsafe { *argv.offset(idx) };
        let mut len = 0isize;
        // SAFETY: `ptr` is a valid null-terminated UTF-16 string.
        while unsafe { *ptr.offset(len) } != 0 {
            len += 1;
        }
        // SAFETY: `ptr[0..len)` are exactly the UTF-16 code units just counted.
        let slice = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
        result.push(OsString::from_wide(slice).to_string_lossy().into_owned());
    }
    // SAFETY: `argv` was allocated by `CommandLineToArgvW` and is freed
    // exactly once, after every element has already been copied out above.
    unsafe {
        LocalFree(argv as *mut core::ffi::c_void);
    }

    let mut it = result.into_iter();
    let executable = it.next().map(PathBuf::from).unwrap_or_default();
    (executable, it.collect())
}

// apply (write path) ==================================================================================================

/// Write `reg` (create or update), then everything outside `ScmRegistration`
/// itself: the metadata blob, failure actions, `env`, and — for a real
/// account — `SeServiceLogonRight`. The blob write comes immediately after
/// create/update, minimizing trap 1's crash window. `current_start_type` is
/// `Discovery::current_start_type` — `None` for a fresh create, `Some(_)`
/// (preserved rather than reset) for an update; see `service_info`.
fn apply(
    spec: &DaemonSpec,
    reg: &ScmRegistration,
    create: bool,
    current_start_type: Option<ServiceStartType>,
) -> Result<()> {
    let password = match spec.user {
        User::Root => None,
        _ => {
            let pw = identity::service_password()?;
            let account = reg
                .account
                .as_deref()
                .expect("a non-Root User always resolves to Some(account) in generate::registration");
            if pw.is_none() && identity::account_needs_password(account) {
                // Trap 4's own failure shape, one step earlier: install
                // would otherwise report success and the service would
                // fail every future start with error 1069. Refuse instead
                // of creating a service that cannot start.
                return Err(Error::Other(format!(
                    "service account `{account}` needs a password (it is not LocalSystem/LocalService/\
                     NetworkService, nor an `NT SERVICE\\`/`NT AUTHORITY\\` account); set \
                     GOETIA_SERVICE_PASSWORD before installing `{}`",
                    spec.id
                )));
            }
            pw
        }
    };

    if create {
        let scm = open_scm(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
        let info = service_info(reg, password, None);
        scm.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::QUERY_CONFIG)
            .map_err(|e| to_error(&format!("create service `{}`", reg.name), e))?;
    } else {
        let scm = open_scm(ServiceManagerAccess::CONNECT)?;
        let service = scm
            .open_service(&reg.name, ServiceAccess::CHANGE_CONFIG | ServiceAccess::QUERY_CONFIG)
            .map_err(|e| to_error(&format!("open service `{}` to update it", reg.name), e))?;
        let info = service_info(reg, password, current_start_type);
        service
            .change_config(&info)
            .map_err(|e| to_error(&format!("update service `{}`", reg.name), e))?;
    }

    registry::write_parameters(&reg.name, &reg.parameters)?;

    // Best-effort, and deliberately here rather than in the shim: it writes
    // under HKLM, and the shim may run as an unprivileged account.
    registry::register_event_source();

    reapply_uncompared_effects(spec, reg)?;

    Ok(())
}

/// Failure actions, `env`, and `SeServiceLogonRight` — everything `apply`
/// writes that is not part of `ScmRegistration`/`render()`, so none of it
/// is covered by `decide::decide`'s comparison (see the module doc
/// comment). A caller that only reaches this via `apply` (i.e. only on
/// `Create`/`Update`/`Stale`) would leave a partial failure of any of these
/// permanently invisible to a retry: once the *compared* surface matches,
/// `decide` reports `UpToDate` forever after, regardless of `--force`.
/// `ServiceManager::install`'s `UpToDate` arm calls this too, so a plain
/// re-`install` converges even without a spec change to force `apply` to
/// run again. (Failure actions ride along here as well: a fresh service's
/// default "no actions" state can coincidentally already match a
/// `None`-desired one, hiding a failed `apply_failure_actions` the same
/// way.)
fn reapply_uncompared_effects(spec: &DaemonSpec, reg: &ScmRegistration) -> Result<()> {
    apply_failure_actions(&reg.name, reg.failure_actions.as_ref())?;
    // `Services\<name>\Environment` is applied by SCM to the *service host*
    // process — the daemon itself for `Kind::Managed`, but `goetia-shim.exe`
    // for `Kind::Simple` (see `generate::registration`'s `Kind::Simple` arm:
    // SCM only ever launches the shim). Writing `spec.env` here for
    // `Kind::Simple` would leak the daemon's configured environment into
    // the SHIM's own process instead of the daemon's — pointless on its
    // own (the shim already applies `spec.env` to the child it spawns, in
    // `src/bin/shim/service.rs`'s `build_command`), and actively harmful if
    // an `env:` key happens to collide with something the shim or its
    // `cosca` dependency reads from its own environment (e.g. `cosca`'s
    // reserved `__COSCA_GROUP_ROOT` nesting marker, which would silently
    // downgrade the shim's own process-tree containment). An empty map for
    // `Kind::Simple` makes `write_environment` delete any existing value
    // rather than write one, so an id whose `type:` flips from `managed`
    // to `simple` still converges.
    let env_for_host = match spec.kind {
        Kind::Managed => spec.env.clone(),
        Kind::Simple => BTreeMap::new(),
    };
    registry::write_environment(&reg.name, &env_for_host)?;
    if let Some(account) = &reg.account {
        identity::grant_service_logon_right(account)?;
    }
    Ok(())
}

/// `ServiceInfo` for `reg`, for either `create_service` or `change_config`.
///
/// **`for_update` matters for `account`.** `windows-service`'s
/// `RawServiceInfo` passes `account_name: None` through as a null
/// `lpServiceStartName` for both APIs — but `CreateServiceW(NULL)` means
/// "run as `LocalSystem`" while `ChangeServiceConfigW(NULL)` means "leave
/// the account unchanged". `reg.account: None` means "we want
/// `LocalSystem`" either way (see `generate::registration`'s own mapping),
/// so on the update path a desired `LocalSystem` must be spelled out as the
/// literal string, or a service whose spec changes from a real account back
/// to `user: root` would silently keep running as the old account.
///
/// **`current_start_type` matters for `start_type`.** `windows-service`'s
/// `change_config` always sends `dwStartType` as a real value, never
/// `SERVICE_NO_CHANGE` — so a create always uses `SERVICE_DEMAND_START`
/// (trap 5: never `SERVICE_DISABLED`/`SERVICE_AUTO_START`), but an update
/// must restate whatever is *already there* (`current_start_type`), or a
/// routine spec-driven `install` would silently reset a `SERVICE_AUTO_START`
/// `enable` had set. `enable`/`disable` themselves go through
/// `set_start_type`'s raw `SERVICE_NO_CHANGE` call instead of this function.
fn service_info(
    reg: &ScmRegistration,
    password: Option<String>,
    current_start_type: Option<ServiceStartType>,
) -> ServiceInfo {
    let for_update = current_start_type.is_some();
    let account_name = match &reg.account {
        Some(a) => Some(OsString::from(a)),
        None if for_update => Some(OsString::from("LocalSystem")),
        None => None,
    };
    ServiceInfo {
        name: OsString::from(&reg.name),
        display_name: OsString::from(&reg.display_name),
        service_type: ServiceType::OWN_PROCESS,
        start_type: current_start_type.unwrap_or(ServiceStartType::OnDemand),
        error_control: ServiceErrorControl::Normal,
        executable_path: reg.executable.clone(),
        launch_arguments: reg.arguments.iter().map(OsString::from).collect(),
        dependencies: vec![],
        account_name,
        account_password: password.map(OsString::from),
    }
}

/// `windows-service`'s two failure-actions setters, applied together so a
/// spec change from `restart: on-failure`/`always` back to `restart: never`
/// actually clears whatever a previous install configured, rather than
/// leaving it in place.
/// Writes failure actions via `sc.exe failure`/`sc.exe failureflag`, not the
/// `ChangeServiceConfig2W(SERVICE_CONFIG_FAILURE_ACTIONS[_FLAG])` WinAPI
/// `windows-service`'s `Service::update_failure_actions`/
/// `set_failure_actions_on_non_crash_failures` wrap.
///
/// **Why not the WinAPI call.** `ChangeServiceConfig2W(SERVICE_CONFIG_FAILURE_ACTIONS)`
/// returned `ERROR_ACCESS_DENIED` from this process on GitHub Actions'
/// Windows runner even with `SeShutdownPrivilege` explicitly enabled via
/// `AdjustTokenPrivileges` beforehand — confirmed empirically; an
/// unresolved upstream report from a mature Windows service tool
/// (winsw#893) hits the identical failure in the identical environment.
/// `sc.exe` — a *separate* process, with its own freshly-derived token —
/// does not: `tests/marker_inertness/scm.rs`'s `scm_parameters_values_survive`
/// probe already calls `sc.exe failure` successfully on this same CI.
/// Shelling out here is the same choice the systemd/launchd backends make
/// for operations only their platform's own CLI tool reliably performs
/// (`systemctl daemon-reload`, `launchctl bootstrap`).
///
/// Read-only queries (`read_failure_actions`) are unaffected — only a
/// *write* can configure `SC_ACTION_REBOOT`, which is the operation the
/// privilege actually exists to gate, and stay on the WinAPI via
/// `windows-service`.
fn apply_failure_actions(name: &str, fa: Option<&GenFailureActions>) -> Result<()> {
    match fa {
        None => {
            run_sc(&["failureflag", name, "0"])?;
            run_sc(&["failure", name, "reset=", "0", "actions=", ""])?;
        }
        Some(fa) => {
            let reset = fa.reset_period.as_secs().to_string();
            let action = format!("restart/{}", fa.delay.as_millis());
            run_sc(&["failure", name, "reset=", &reset, "actions=", &action])?;
            // `SERVICE_CONFIG_FAILURE_ACTIONS_FLAG` — see
            // `GenFailureActions::on_non_crash_failures`'s doc comment for
            // why this must be set, not merely the actions themselves.
            run_sc(&["failureflag", name, if fa.on_non_crash_failures { "1" } else { "0" }])?;
        }
    }
    Ok(())
}

fn run_sc(args: &[&str]) -> Result<()> {
    let output = std::process::Command::new("sc.exe")
        .args(args)
        .output()
        .map_err(|e| Error::Other(format!("spawn `sc.exe {}`: {e}", args.join(" "))))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "`sc.exe {}` failed ({:?}): {}{}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )))
    }
}

// uninstall ===========================================================================================================

/// Stop (waiting for a confirmed `STOPPED`, closing that handle), then
/// delete via a fresh handle. See trap 3.
fn uninstall_locked(scm: &WinServiceManager, id: &Id) -> Result<()> {
    let needs_stop = {
        let service = scm
            .open_service(id.as_str(), ServiceAccess::QUERY_STATUS)
            .map_err(|e| to_error(&format!("query status for `{id}` before uninstall"), e))?;
        let status = service
            .query_status()
            .map_err(|e| to_error(&format!("query status for `{id}` before uninstall"), e))?;
        status.current_state != WinState::Stopped
        // `service`'s `SC_HANDLE` closes here, at the end of this block.
    };

    if needs_stop {
        let mut actor = scm_wait::SystemScmActor::open(id.as_str())
            .map_err(|e| Error::Other(format!("open `{id}` to stop it before uninstall: {e}")))?;
        scm_wait::stop_via_notify(&mut actor).map_err(|e| {
            Error::Other(format!(
                "`{id}` did not confirm SERVICE_STOPPED before uninstall ({e}); it was NOT deleted — \
                 DeleteService on a running service only marks it for deletion, which the next install would \
                 meet as ERROR_SERVICE_MARKED_FOR_DELETE. Stop it (or wait for it to stop) and re-run \
                 `goetia daemon uninstall {id}`."
            ))
        })?;
        // `actor`'s `SC_HANDLE` must close before the `DELETE` handle opens
        // below — dropping it explicitly documents that ordering rather than
        // relying on it falling out of scope at the end of the function.
        drop(actor);
    }

    let service = scm
        .open_service(id.as_str(), ServiceAccess::DELETE)
        .map_err(|e| to_error(&format!("open `{id}` to delete it"), e))?;
    service
        .delete()
        .map_err(|e| to_error(&format!("delete service `{id}`"), e))
}

// shared helpers ======================================================================================================

fn open_scm(access: ServiceManagerAccess) -> Result<WinServiceManager> {
    WinServiceManager::local_computer(None::<&str>, access).map_err(|e| to_error("open the Service Control Manager", e))
}

fn is_not_found(e: &windows_service::Error) -> bool {
    matches!(
        e,
        windows_service::Error::Winapi(io) if io.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32)
    )
}

/// `windows_service::Error`'s own `Display` for its `Winapi` variant is the
/// literal string `"IO error in winapi call"` — it does not include the
/// wrapped `io::Error`'s message at all (verified: every error surfaced
/// through `to_error` read that and nothing else, useless for diagnosing an
/// actual Win32 failure). Unwrap to the inner `io::Error` first, whose
/// `Display` does carry `FormatMessage`'s real text.
fn to_error(context: &str, e: windows_service::Error) -> Error {
    match e {
        windows_service::Error::Winapi(io_err) => Error::Other(format!("{context}: {io_err}")),
        other => Error::Other(format!("{context}: {other}")),
    }
}

fn not_installed(id: &Id) -> Error {
    Error::NotInstalled {
        id: id.as_str().to_string(),
    }
}

fn foreign(id: &Id) -> Error {
    Error::Foreign {
        id: id.as_str().to_string(),
        recovery: decide::foreign_recovery(id.as_str()),
    }
}

/// Open `id` with `access`, translating "no such service" into
/// [`Error::NotInstalled`] rather than a raw Win32 message — every verb but
/// `install` needs this before it can even ask whether `id` is *ours* (see
/// [`require_ours`]).
fn open_existing(id: &Id, access: ServiceAccess) -> Result<(WinServiceManager, Service)> {
    let scm = open_scm(ServiceManagerAccess::CONNECT)?;
    let service = scm.open_service(id.as_str(), access).map_err(|e| {
        if is_not_found(&e) {
            not_installed(id)
        } else {
            to_error(&format!("open service `{id}`"), e)
        }
    })?;
    Ok((scm, service))
}

/// The narrower "is this even ours" gate every verb but `install`/`status`
/// needs (see `manager::fake`'s `require_ours`, which this mirrors exactly):
/// a marker that fails to *decode* still passes — `uninstall`'s `recovery`
/// text (`decide::Outcome::RefuseUnreadable`) promises exactly this, since
/// forcing a decode here would turn a corrupted `Spec` value into an id no
/// verb can ever touch again. Only a wholly absent `Marker` — a foreign
/// service, or (trap 1) a `type: managed` install interrupted before this
/// backend's one ownership proof was ever written — is refused.
fn require_ours(id: &Id) -> Result<()> {
    let params = registry::read_parameters(id.as_str())?;
    match generate::extract(&params) {
        Ok(None) => Err(foreign(id)),
        Ok(Some(_)) | Err(_) => Ok(()),
    }
}

fn map_state(s: WinState) -> State {
    match s {
        WinState::Running => State::Running,
        // SCM has no state distinct from `Stopped` for "stopped because it
        // failed" — `dwWin32ExitCode`/`dwServiceSpecificExitCode` carry that,
        // not `dwCurrentState`. Recovery actions may already have restarted
        // it by the time this is read, in any case.
        WinState::Stopped => State::Stopped,
        WinState::StartPending
        | WinState::StopPending
        | WinState::ContinuePending
        | WinState::PausePending
        | WinState::Paused => State::Unknown,
    }
}

fn query_live(name: &str) -> Result<(State, bool)> {
    let scm = open_scm(ServiceManagerAccess::CONNECT)?;
    let service = scm
        .open_service(name, ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG)
        .map_err(|e| to_error(&format!("open `{name}` to query its live status"), e))?;
    let status = service
        .query_status()
        .map_err(|e| to_error(&format!("query status for `{name}`"), e))?;
    let cfg = service
        .query_config()
        .map_err(|e| to_error(&format!("query configuration for `{name}`"), e))?;
    Ok((
        map_state(status.current_state),
        cfg.start_type == ServiceStartType::AutoStart,
    ))
}

/// Flip `dwStartType` alone, via a raw `ChangeServiceConfigW` with
/// `SERVICE_NO_CHANGE` on every other field — `windows-service`'s
/// `change_config` has no partial-update mode and would otherwise require
/// restating (and risk drifting) every other field `enable`/`disable` has
/// no business touching.
fn set_start_type(service: &Service, start_type: ServiceStartType) -> std::io::Result<()> {
    // SAFETY: every parameter but `hservice` and `dwstarttype` is
    // `SERVICE_NO_CHANGE` or null, so this call changes only the start type.
    let ok = unsafe {
        ChangeServiceConfigW(
            service.raw_handle(),
            SERVICE_NO_CHANGE,
            start_type.to_raw(),
            SERVICE_NO_CHANGE,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod manager_tests;

/// One line, not one per service.
///
/// Most services on a Windows box carry an ACL that denies a non-elevated read
/// of their `Parameters`, so warning per service buries `goetia daemon list` in
/// noise about services that were never ours. Staying silent would be the
/// opposite lie: a denied read means ownership is *unknown*, so one of ours
/// could be missing from the listing.
fn unreadable_notice(count: usize) -> String {
    let s = if count == 1 { "" } else { "s" };
    format!(
        "{count} service{s} could not be inspected (access denied reading registry Parameters). Ownership is unknown for {}, so a Goetia daemon may be missing from this list; re-run elevated.",
        if count == 1 { "it" } else { "them" }
    )
}

//! The effectful SCM backend (`#[cfg(windows)]`).
//!
//! Handles both `Kind::Simple` (under the host the manager's
//! [`ShimProvider`](crate::shim::ShimProvider) supplies, which runs `shim::run`) and
//! `Kind::Managed`; `generate::registration` builds the `ImagePath`/blob
//! for both. `Kind::Simple`'s own integration tests live in
//! `tests/shim_integration.rs`, not `tests/scm_integration.rs`, because they
//! need `CARGO_BIN_EXE_goetia-shim`.
//!
//! ## The five traps this module addresses
//!
//! **1. Ownership has only one proof for `type: managed`.** `type: simple`'s
//! `ImagePath` (the service host's command, written atomically by `CreateServiceW`)
//! is a second proof of ownership independent of `Parameters`; `type: managed`'s
//! `ImagePath` is just the daemon's own command, indistinguishable from any
//! other service that happens to run the same executable, so it cannot serve
//! that role. This means the crash window between `CreateServiceW` and the
//! `Parameters` write (see point 5) has no in-tool escape route for
//! `type: managed` the way it does for `type: simple`: `read` classifies
//! such an orphan as foreign (no marker at all), and the next
//! `install` refuses with [`crate::decide::foreign_recovery`]'s named command — the
//! real Windows analogue of `sc.exe delete <id>` (or Services MMC), which is
//! in fact the exact remedy: nothing but deleting the orphan and re-running
//! `install` can recover it. `interrupted_install_is_recoverable` in
//! `tests/scm_integration.rs` pins this. `uninstall`, meanwhile, still does
//! not need a *decodable* blob — only the `Marker` field, so a corrupted
//! (but present) `Spec` value never wedges an id.
//!
//! **2. Stopping (and starting) must not poll.** `windows-service`'s
//! `stop()`/`start()` return as soon as SCM accepts the request, not once
//! the transition completes, and the crate wraps no wait primitive. See
//! `super::wait`, which uses `NotifyServiceStatusChangeW` — a real kernel
//! rendezvous, never a `Sleep`+`QueryServiceStatusEx` poll.
//!
//! **3. Uninstall must disable, stop what runs, then delete.**
//! `DeleteService` on a running service only *marks* it for deletion: the record survives until
//! every `SC_HANDLE` closes and the service stops, so the next `install` would meet
//! `ERROR_SERVICE_MARKED_FOR_DELETE`. A restart policy adds a race: SCM can start the service
//! between any read of its state and a later call. [`uninstall_locked`] therefore opens its wait
//! actor first, sets the start type to `SERVICE_DISABLED` (a disabled service's recovery restart
//! fails with `ERROR_SERVICE_DISABLED` and `StartService` is refused, so a restart queued or made
//! after the disable starts nothing; a disable that lands once an attempt has begun does not stop
//! it, P23), stops whatever runs, including a start that had already begun, and only
//! then closes the actor and deletes through a fresh `DELETE` handle. The stop never relies on an earlier observation: it
//! is sent on a notification and, when SCM rejects it because the state changed, the state is read
//! again. The service is not marked during the stop, so notifications keep working on it. A failure
//! before the delete leaves the service in place and disabled, and re-running `uninstall` is the
//! retry. A service that ran a shim file also has that file to clean up: `uninstall` holds the
//! install lock, holds the host's process before the stop, waits for it after the delete, and then
//! sweeps what nothing references (see `Scm::finish_uninstall`).
//!
//! **4. A real account needs `SeServiceLogonRight`.** `CreateServiceW`
//! succeeds without it; the first start then fails with error 1069
//! (`ERROR_LOGON_FAILURE`) while `install` has already reported success. See
//! `identity::grant_service_logon_right`, called before the service is
//! created or changed, for every account [`generate::account_needs_password`]
//! says needs a password — every other account (`LocalSystem`, a built-in, or an
//! `NT SERVICE\<id>` virtual account) already holds the right, and
//! `LookupAccountNameW` (which the grant goes through) does not even
//! resolve the first three by those spellings.
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
//! environment of the service process at launch. It is part of
//! `ScmRegistration`'s `Display` text, so a hand-edit to `Environment` alone is a
//! conflict, and an up-to-date install writes nothing.
//! `managed_kind_environment_availability` in `tests/scm_integration.rs`
//! verifies the mechanism empirically, on the real (elevated) Windows CI
//! runner.
//!
//! ## Passwords
//!
//! Per the design spec's "Windows accounts" section, a real (non-built-in,
//! non-virtual) account needs a password, supplied via the
//! `GOETIA_SERVICE_PASSWORD` environment variable — never `goetia.yaml`. See
//! `identity::service_password`. `apply` never even reads that variable for
//! a built-in or virtual account (see `generate::account_needs_password`):
//! a value left over from installing a different daemon must not silently
//! reach `CreateServiceW`/`ChangeServiceConfigW` for one that needs none —
//! Windows rejects a password outright for those accounts.

pub(in crate::backend::scm) mod identity;
pub(in crate::backend::scm) mod registry;
#[cfg(any(test, feature = "testing"))]
pub(crate) use identity::sid_for_account_name;

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use windows_service::service::{
    Service, ServiceAccess, ServiceActionType, ServiceConfig, ServiceErrorControl, ServiceFailureResetPeriod,
    ServiceInfo, ServiceStartType, ServiceState as WinState, ServiceType,
};
use windows_service::service_manager::{ServiceManager as WinServiceManager, ServiceManagerAccess};
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SERVICE_MARKED_FOR_DELETE, LocalFree};
use windows_sys::Win32::System::Services::{ChangeServiceConfigW, SERVICE_NO_CHANGE};
use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

use self::registry::RegistryReadOutcome;
use super::exit_status::ERROR_SERVICE_DOES_NOT_EXIST;
use super::wait::{EndProcessOutcome, StopAndAwaitExitOutcome, Waited, end_process, is_incomplete_stop};
use crate::backend::scm::generate::{self, FailureActions as GenFailureActions, ScmRegistration};
use crate::backend::scm::kill::system::Held;
use crate::backend::scm::kill::{HoldOutcome, hold_if_possible};
use crate::backend::scm::parameters::Parameters;
use crate::backend::scm::shim::acl::{CheckPermissions, FolderCheck};
use crate::backend::scm::shim::embedded::{ShimFile, folder_in, shim_file};
use crate::backend::scm::shim::lock::InstallLock;
use crate::backend::scm::shim::marker;
use crate::backend::scm::shim::provider::{ShimProvider, require_absolute};
use crate::backend::scm::shim::sweep::{
    ProviderFolder, SweepFailure, left_behind_text, normalise, referenced_hosts, second_folder, sweep_folder,
};
use crate::backend::{
    Backend, Decoded, InstalledDefinition, InstalledService, ReadOutcome, Scan, ScanEntry, Unnamed, WriteOutcome,
};
use crate::blob::Blob;
use crate::decide::{ExternalConfig, Outcome};
use crate::error::{Chain, Error, Reason, Result};
use crate::manager::budget::{self, Deadline};
use crate::manager::{Budget, Installed, State, Status, Step};
use crate::shim::HostCommand;
use crate::spec::{DaemonSpec, Id, Kind};

// Scm =================================================================================================================

/// The Windows service manager. Holds no state of its own — every operation opens exactly the
/// SCM/registry handles it needs and closes them again — only the source of `type: simple`
/// services' host and the check on the folder it lives in.
pub struct Scm {
    /// `None` for [`Scm::new`]: no host to provide, and no files of the manager's own to sweep.
    shim: Option<Box<dyn ShimProvider>>,
    folders: FolderCheck,
}

impl Scm {
    /// A manager with no shim provider: `type: managed` services work, a `type: simple` one cannot
    /// be installed or previewed ([`Error::NoShimProvider`]), and `status` and `list` judge what is
    /// installed without one.
    pub fn new() -> Self {
        Self {
            shim: None,
            folders: FolderCheck::new(CheckPermissions::Enable),
        }
    }

    /// A manager whose `type: simple` services run the host `provider` supplies. `check` says
    /// whether the folder of what `provider` returns is checked for permissions that would let
    /// another account replace it. It also decides which folders `provider`'s sweep may list and
    /// delete in: a sweep checks the folder with it too.
    pub fn with_shim(provider: impl ShimProvider + 'static, check: CheckPermissions) -> Self {
        Self {
            shim: Some(Box::new(provider)),
            folders: FolderCheck::new(check),
        }
    }

    /// The provider, or [`Error::NoShimProvider`] for a manager built without one.
    fn provider(&self) -> Result<&dyn ShimProvider> {
        self.shim.as_deref().ok_or(Error::NoShimProvider)
    }

    /// The host `spec` would be installed with, as the provider previews it: `Some` for `type: simple`
    /// only, which has a host.
    fn host_of(&self, spec: &DaemonSpec) -> Result<Option<HostCommand>> {
        match spec.kind {
            Kind::Simple => Ok(Some(self.provider()?.preview()?)),
            Kind::Managed => Ok(None),
        }
    }
}

impl Default for Scm {
    fn default() -> Self {
        Self::new()
    }
}

/// What `read` saw of a service goetia marked.
pub(crate) struct ScmSnapshot {
    id: Id,
    blob: Decoded,
    /// Its `Parameters` values, which hold the only proof of ownership `type: managed` has.
    parameters: Parameters,
    /// The one object every query of this snapshot asks, so that a service deleted and recreated
    /// meanwhile cannot answer part of them: the one `read` opened to classify it, or, for what
    /// `scan` saw (which opens no service), the one [`Self::service`] opens on first use.
    service: OnceLock<Service>,
    /// The service's configuration, queried by whichever of `installed`, `regenerate` and
    /// `live_status` asks first and kept: a verb that needs it twice (`judge` reads what is
    /// installed, then regenerates against it) cannot fail on the second read.
    config: OnceLock<ServiceConfig>,
}

/// What `read_marker` hands on of a service: its `Parameters` and the object `read` opened.
type Opened = (Parameters, Option<Service>);

impl ScmSnapshot {
    fn new(id: Id, blob: Decoded, (parameters, opened): Opened) -> Self {
        let service = OnceLock::new();
        if let Some(opened) = opened {
            // A fresh cell takes the first value.
            let _ = service.set(opened);
        }
        Self {
            id,
            blob,
            parameters,
            service,
            config: OnceLock::new(),
        }
    }
}

impl InstalledService for ScmSnapshot {
    fn id(&self) -> &Id {
        &self.id
    }

    fn blob(&self) -> std::result::Result<&Blob, &Arc<Error>> {
        self.blob.as_ref()
    }
}

impl ScmSnapshot {
    /// The snapshot's service: the one `read` opened, or one opened by name on first use. A service
    /// gone by then is [`Error::NotInstalled`].
    fn service(&self) -> Result<&Service> {
        if let Some(service) = self.service.get() {
            return Ok(service);
        }
        let id = &self.id;
        let service = open_scm(ServiceManagerAccess::CONNECT)?
            .open_service(id.as_str(), ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG)
            .map_err(|e| {
                if is_not_found(&e) || is_marked_for_delete(&e) {
                    Error::NotInstalled {
                        id: id.as_str().to_string(),
                    }
                } else {
                    to_error(&format!("open `{id}` to query it"), e)
                }
            })?;
        Ok(self.service.get_or_init(|| service))
    }

    /// The service's configuration, queried on the first call and kept.
    fn config(&self) -> Result<&ServiceConfig> {
        if let Some(config) = self.config.get() {
            return Ok(config);
        }
        let id = &self.id;
        let config = self
            .service()?
            .query_config()
            .map_err(|e| to_error(&format!("query configuration for `{id}`"), e))?;
        Ok(self.config.get_or_init(|| config))
    }

    /// The id, with the service `read` opened closed: a verb that deletes the service, or waits on
    /// it through a handle of its own, must not leave this one open.
    fn into_id(self) -> Id {
        self.id
    }
}

impl Backend for Scm {
    type Definition = ScmRegistration;
    type Snapshot = ScmSnapshot;
    /// The install lock, which `type: simple` holds until its install ends: what places, repairs
    /// and deletes shim files, for this process and every other, takes it.
    type InstallScope = Option<InstallLock>;
    type Reservation = ();

    /// For `type: simple`: the install flow has rendered the definition already, so every refusal
    /// `render_for_install` can make (the previewed host's path, an account, `restart: always`)
    /// has come before the lock is taken or the provider writes. The lock is taken and the files
    /// nothing references are swept (by the provider, in a folder that exists and passes the folder check: see
    /// [`sweep_folder`]), and then the host is provided and checked before anything is
    /// read: `provide` must return the host `desired` registers (the flow's render, from the
    /// provider's `preview`: a sibling shim upgraded while the install waited on the lock
    /// would otherwise register a file that was never placed), an absolute path that passes the
    /// folder check (which walks it and its ancestors), to a file carrying this library's marker. A
    /// sweep that cannot read what is referenced or cannot delete a file does not fail the install:
    /// the file is not what the install needs, and the next sweep finds it. `type: managed` has no
    /// host, so nothing is held or checked: its preconditions are `apply`'s, before any write.
    fn begin_install(&self, spec: &DaemonSpec, desired: &ScmRegistration) -> Result<Option<InstallLock>> {
        if spec.kind != Kind::Simple {
            return Ok(None);
        }
        let shim = self.provider()?;
        let lock = InstallLock::acquire()?;
        if let Ok(referenced) = referenced_hosts() {
            let _ = shim.sweep(&referenced, &self.folders);
        }
        let provided = shim.provide(&self.folders)?;
        let rendered = HostCommand {
            exe: desired.executable.clone(),
            args: desired.arguments.clone(),
        };
        let provided = confirm_provided(rendered, provided)?;
        require_absolute(&provided.exe)?;
        self.folders.check(&provided.exe)?;
        marker::verify(&provided.exe)?;
        Ok(Some(lock))
    }

    /// SCM sends its requests through the OS, with no thread or file of ours to run out.
    fn reserve(&self, _steps: &[Step], _budget: Budget) -> Result<()> {
        Ok(())
    }

    fn render(&self, spec: &DaemonSpec) -> Result<ScmRegistration> {
        let identity = identity::resolve(&spec.user)?;
        generate::registration(spec, &identity, self.host_of(spec)?.as_ref())
    }

    /// Keeps the host command a `type: simple` service is installed with, so that a host another
    /// provider (a moved folder, another goetia version, a standalone host) put there is the same
    /// spec, and no provider is consulted. `type: managed` has no host.
    fn regenerate(&self, spec: &DaemonSpec, snapshot: &ScmSnapshot) -> Result<ScmRegistration> {
        if spec.kind == Kind::Managed {
            return self.render(spec);
        }
        let identity = identity::resolve(&spec.user)?;
        let config = snapshot.config().map_err(|e| Error::InstalledUnreadable {
            id: snapshot.id().as_str().to_string(),
            source: Box::new(e),
        })?;
        let (exe, args) = split_command_line(&config.executable_path.to_string_lossy());
        generate::registration(spec, &identity, Some(&HostCommand { exe, args }))
    }

    /// The host is previewed once, for the check and the registration alike.
    fn render_for_install(&self, spec: &DaemonSpec) -> Result<ScmRegistration> {
        let identity = identity::resolve(&spec.user)?;
        generate::install_registration(spec, &identity, self.host_of(spec)?.as_ref())
    }

    /// Upstream of the marker, so a service that will not open leaves `id` undetermined: neither
    /// absent nor goetia's.
    fn read(&self, id: &Id) -> Result<ReadOutcome<ScmSnapshot>> {
        let scm = open_scm_to_classify(id.as_str())?;
        let service = match scm.open_service(id.as_str(), ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG) {
            Ok(service) => service,
            Err(e) if is_not_found(&e) => return Ok(ReadOutcome::Absent),
            Err(e) => return Err(service_undetermined(id.as_str(), &format!("open service `{id}`"), e)),
        };
        let parameters = held_service(id.as_str(), registry::read_parameters(id.as_str())?)?;
        Ok(read_marker(parameters, Some(service)).named(id, ScmSnapshot::new))
    }

    /// SCM has no drop-in mechanism: everything goetia writes lives under the service's own key.
    fn installed(&self, snapshot: &ScmSnapshot) -> Result<InstalledDefinition<ScmRegistration>> {
        let id = snapshot.id();
        let config = snapshot.config()?;
        let definition = read_live_registration(config, snapshot.service()?, id.as_str(), &snapshot.parameters)?;
        Ok(InstalledDefinition {
            definition,
            external: ExternalConfig::Absent,
        })
    }

    fn write(
        &self,
        _spec: &DaemonSpec,
        definition: &ScmRegistration,
        _outcome: &Outcome,
        snapshot: Option<&ScmSnapshot>,
        _scope: &mut Option<InstallLock>,
        _reservation: &mut (),
    ) -> Result<WriteOutcome> {
        apply(definition, snapshot.is_none())?;
        Ok(WriteOutcome::Done)
    }

    fn live_status(&self, snapshot: &ScmSnapshot) -> Result<Status> {
        let id = snapshot.id();
        let enabled = snapshot.config()?.start_type == ServiceStartType::AutoStart;
        let (state, pid) = query_live(snapshot.service()?, id.as_str())?;
        Ok(Status { state, pid, enabled })
    }

    /// Read directly from the registry rather than `windows-service`'s enumeration API:
    /// `SC_MANAGER_ENUMERATE_SERVICE`/`EnumServicesStatusExW` need no more privilege than reading
    /// this key does, and one pass here avoids an `OpenService` round trip per candidate on a host
    /// with hundreds of unrelated services.
    fn scan(&self) -> Scan<ScmSnapshot> {
        let mut entries = Vec::new();
        let mut unreadable: Vec<Unreadable> = Vec::new();
        let incomplete = registry::scan_services(|name, services| {
            // A registry read failure for one unrelated service (e.g. a driver whose `Parameters`
            // key carries a restrictive ACL) must not take `list` down for every other daemon. It
            // leaves us unable to tell whether `name` carries goetia's marker at all, so it is
            // neither omitted nor claimed: collected whole here, reported by
            // `unreadable_aggregate`, which names the id, the failure and its remedy when it turns
            // out to be the only one.
            match registry::read_parameters_in(services, &name) {
                // Gone since `scan_services` listed it: not there to list. A `Parameters` key not
                // found, marked deleted or gone mid-read reaches this only if the service key is
                // gone too.
                Ok(RegistryReadOutcome::ServiceGone) => {}
                Ok(RegistryReadOutcome::Read(params)) => {
                    let entry = ScanEntry::of(&name, read_marker(params, None), ScmSnapshot::new);
                    entries.push((name, entry));
                }
                Err(e) => unreadable.push(unreadable_parts(name, e)),
            }
        });
        // Two aggregates, kept apart: the first stands for services this pass reached and could
        // not read, the second for services it never reached. Both carry no name, so either alone
        // forbids concluding any id absent; separately, each count stays honest.
        Scan {
            entries,
            reported: unreadable_aggregate(unreadable).into_iter().chain(incomplete).collect(),
        }
    }

    /// A service that may run a shim file takes the install lock first and keeps it to the end, so
    /// that no install places or sweeps a file between the stop and the sweep. If the service's host
    /// is a shim file by name, its process is held after the service is disabled, before the stop or
    /// the wait for one under way (see [`uninstall_locked`]), and once the service is stopped
    /// and deleted the uninstall waits for that process and sweeps the folders the files can be in:
    /// see [`Scm::finish_uninstall`]. Anything else is [`uninstall_locked`] alone.
    fn uninstall(&self, snapshot: ScmSnapshot, _reservation: &mut ()) -> Result<()> {
        // A blob that will not decode leaves the kind open: the host's name then decides.
        let may_run_a_shim = snapshot.blob().map_or(true, |blob| blob.spec.kind == Kind::Simple);
        // First, so that the host read below is not one an install in progress is about to change.
        let lock = may_run_a_shim.then(InstallLock::acquire).transpose()?;
        let host = if lock.is_some() {
            registered_shim(&snapshot)
        } else {
            Ok(None)
        };
        // A host that could not be read is not "no shim": the uninstall goes on, since the service
        // is deleted either way, and says that the cleanup was skipped, and why, if it succeeds.
        let (host, unread) = match host {
            Ok(host) => (host, None),
            Err(e) => (None, Some(e)),
        };
        // `uninstall_locked` opens its own handles, and its delete needs this one closed: an open
        // handle leaves the service only marked for deletion.
        let shim = runs_a_shim(&snapshot);
        let id = snapshot.into_id();
        let scm = open_scm_for(&id, "uninstall").map_err(|e| uninstall_failed(&id, &UninstallFailure::Untouched, e))?;
        match uninstall_locked(&scm, &id, shim, host.is_some())? {
            Uninstalled::Deleted { process } => match (host, unread) {
                (Some(host), _) => self.finish_uninstall(&id, &host, process),
                (None, Some(unread)) => Err(self.cleanup_skipped(&id, unread)),
                (None, None) => Ok(()),
            },
            Uninstalled::Left => Ok(()),
        }
    }

    /// Under the lock, the sweep an `install` makes before it provides, which is how an `uninstall`
    /// that stopped short of its own sweep is retried: by running it again. A manager with no
    /// provider has nothing of its own to sweep, and takes no lock and reads nothing.
    fn uninstall_absent(&self, _id: &Id) -> Result<()> {
        let Some(shim) = &self.shim else {
            return Ok(());
        };
        let _lock = InstallLock::acquire()?;
        shim.sweep(&referenced_hosts()?, &self.folders)
            .map_err(|failure| *failure.error)
    }

    fn enable(&self, snapshot: ScmSnapshot) -> Result<()> {
        let id = snapshot.id();
        let (_scm, service) = open_existing(id, "enable", ServiceAccess::CHANGE_CONFIG)?;
        set_start_type(&service, ServiceStartType::AutoStart).map_err(|e| Error::os(format!("enable `{id}`"), e))
    }

    fn disable(&self, snapshot: ScmSnapshot) -> Result<()> {
        let id = snapshot.id();
        let (_scm, service) = open_existing(id, "disable", ServiceAccess::CHANGE_CONFIG)?;
        set_start_type(&service, ServiceStartType::OnDemand).map_err(|e| Error::os(format!("disable `{id}`"), e))
    }

    fn start(&self, snapshot: ScmSnapshot, budget: Budget, _reservation: &mut ()) -> Result<()> {
        // First, so the wait is counted from the verb's entry. What it bounds is the wait:
        // `start_via_notify` issues the start whatever is left of it.
        let deadline = budget.start();
        let (kind, unreadable) = start_kind(snapshot.blob());
        let id = snapshot.into_id();
        let mut actor = super::wait::SystemScmActor::open(id.as_str())
            .map_err(|e| Error::os(format!("open `{id}` to start it"), e))?;
        if !budget.waits() {
            // Request-only: `StartServiceW` and return, arming no wait at
            // all. `Ok(())` then means the request was accepted and nothing
            // more, per the trait doc comment.
            return super::wait::request_start(&mut actor).map_err(|e| Error::os(format!("start `{id}`"), e));
        }
        let waited =
            super::wait::start_via_notify(&mut actor, deadline, kind).map_err(|e| failed_start(&id, e, unreadable))?;
        match waited {
            Waited::Confirmed => Ok(()),
            Waited::Expired => Err(budget::timed_out(id.as_str(), "running", budget)),
        }
    }

    /// [`super::wait::request_start_after_stop`]: every
    /// `ERROR_SERVICE_ALREADY_RUNNING` is a refusal here, since a `type:
    /// simple` service reads `RUNNING` until its shim has taken the stop.
    fn request_start_after_stop(&self, snapshot: ScmSnapshot, _reservation: &mut ()) -> Result<()> {
        let id = snapshot.into_id();
        let mut actor = super::wait::SystemScmActor::open(id.as_str())
            .map_err(|e| Error::os(format!("open `{id}` to start it"), e))?;
        super::wait::request_start_after_stop(&mut actor).map_err(|e| Error::os(format!("start `{id}`"), e))
    }

    /// With a wait, holds the service's process before the STOP, and once SCM reports the service
    /// stopped ends that process through its handle (the documentation of `SetServiceStatus` allows
    /// it after `SERVICE_STOPPED`) and waits, within the budget, for it to exit, so the file it
    /// runs is free when `stop` returns. A service whose shim recorded an incomplete stop (the daemon's
    /// tree may still be running) is a failed stop, the process not ended; `uninstall` fails the same
    /// way, and leaves the service stopped, disabled and not deleted (see [`runs_a_shim`]).
    ///
    /// A process that cannot be opened is not waited for. A `stop` that succeeds has no channel to
    /// say so; one that fails leads its error with it (see [`unwaited`]).
    ///
    /// The gap: if the held process crashes and Windows' recovery restarts the service before the
    /// STOP is sent, the STOP reaches the new process, which goetia does not hold. `stop` can then
    /// return while that process is still exiting, and an `uninstall` right after may leave the shim
    /// file for the next sweep.
    fn stop(&self, snapshot: ScmSnapshot, budget: Budget, _reservation: &mut ()) -> Result<()> {
        // First, so the wait is counted from the verb's entry; `stop_via_notify` issues the stop
        // whatever is left of it.
        let deadline = budget.start();
        let shim = runs_a_shim(&snapshot);
        let id = snapshot.into_id();
        let mut actor = super::wait::SystemScmActor::open(id.as_str())
            .map_err(|e| Error::os(format!("open `{id}` to stop it"), e))?;
        if !budget.waits() {
            return super::wait::request_stop(&mut actor).map_err(|e| Error::os(format!("stop `{id}`"), e));
        }
        // Before the STOP: SCM reports `STOPPED` before the process exits, and no pid after it. An
        // expired hold is sent the STOP all the same, and is not a stop: see `stop_and_await_exit`.
        let mut held = hold_process(&id, "stop", deadline, &format!("`{id}` was NOT stopped"))?;
        let stopped = super::wait::stop_and_await_exit(&mut actor, deadline, &mut held, shim, |held, deadline| {
            end_held(&id, held, deadline)
        })
        .map_err(|e| Error::os(format!("stop `{id}`"), e))
        .and_then(|outcome| match outcome {
            StopAndAwaitExitOutcome::Stopped => Ok(()),
            // The STOP was sent and its effect is not confirmed, as when SCM does not answer in time.
            StopAndAwaitExitOutcome::ServiceExpired => Err(budget::timed_out(id.as_str(), "stopped", budget)),
            StopAndAwaitExitOutcome::Unconfirmed { pid } => Err(unconfirmed_stop(&id, pid, budget)),
            StopAndAwaitExitOutcome::ProcessExpired {
                process,
                terminate_failed,
            } => Err(budget::process_timed_out(
                id.as_str(),
                process.pid(),
                budget,
                terminate_failed,
            )),
        });
        // A process that cannot be opened is not waited for (see `stop_and_await_exit`), and a `stop`
        // that succeeds has no channel to say so: the reason leads the error of one that fails.
        stopped.map_err(|error| match held {
            HoldOutcome::Refused(reason) => unwaited(&id, reason, error),
            _ => error,
        })
    }

    /// See [`super::kill::kill_service`] for how the process is chosen safely. Exit code `1`
    /// (`TerminateProcess`): SCM sees an unexpected exit and applies the recovery actions, as for a
    /// crash. For `type: simple` the process is `goetia-shim`, whose Job Object takes the daemon's
    /// tree with it.
    fn kill(&self, snapshot: ScmSnapshot, _reservation: &mut ()) -> Result<()> {
        let id = snapshot.into_id();
        let (_scm, service) = open_existing(&id, "kill", ServiceAccess::QUERY_STATUS)?;
        let mut target = super::kill::SystemKillTarget::new(service, id.as_str().to_string());
        match super::kill::kill_service(&mut target).map_err(|e| Error::os(format!("kill `{id}`"), e))? {
            super::kill::Killed::Terminated => Ok(()),
            super::kill::Killed::NotRunning => Err(Error::NotRunning {
                id: id.as_str().to_string(),
            }),
        }
    }
}

/// `provided`, the host `provide` returned, when it is `rendered`, the host the registration names;
/// otherwise [`Error::ShimHostMismatch`], which does not say why (a changed host and a provider
/// whose `preview` and `provide` disagree look alike from here).
fn confirm_provided(rendered: HostCommand, provided: HostCommand) -> Result<HostCommand> {
    if provided == rendered {
        return Ok(provided);
    }
    Err(Error::ShimHostMismatch {
        rendered: Box::new(rendered),
        provided: Box::new(provided),
    })
}

/// A registry read of a service SCM has open: SCM says the service exists, so a registry that finds
/// its key gone disagrees with SCM, and the read failed. The service is neither absent nor foreign.
fn held_service<T>(id: &str, read: RegistryReadOutcome<T>) -> Result<T> {
    match read {
        RegistryReadOutcome::Read(value) => Ok(value),
        RegistryReadOutcome::ServiceGone => Err(registry::missing_service_key(id)),
    }
}

/// What the `Parameters` of a service goetia could open say about its ownership. No marker is
/// foreign, with the stock remedy: nothing but deleting the orphan recovers it, since `type: managed`
/// has no second proof of ownership.
fn read_marker(parameters: Parameters, service: Option<Service>) -> ReadOutcome<Unnamed<Opened>> {
    let blob = match generate::extract(&parameters.fields, &parameters.strays) {
        Ok(None) => return ReadOutcome::Foreign { recovery: None },
        Ok(Some(blob)) => Ok(blob),
        Err(e) => Err(Arc::new(e)),
    };
    ReadOutcome::Marked(Unnamed {
        blob,
        body: (parameters, service),
    })
}

// shim support ========================================================================================================

/// Read and decode the metadata blob for `id` directly from
/// `Services\<id>\Parameters`. The shim calls this: it knows only its service
/// name, not a `DaemonSpec` to hand to `render`, and must not pull in
/// `ServiceManager`'s effectful install path just to read one value.
///
/// A service that is gone is [`Error::NotInstalled`]. `Ok(None)` means no goetia `Marker` (none, or
/// another service's own), kept apart from `Err` so callers can tell it from an undecodable spec.
pub(crate) fn read_spec_blob(id: &str) -> Result<Option<Blob>> {
    match registry::read_parameters(id)? {
        RegistryReadOutcome::ServiceGone => Err(Error::NotInstalled { id: id.to_string() }),
        RegistryReadOutcome::Read(params) => generate::extract(&params.fields, &params.strays),
    }
}

/// The kind `start_via_notify` reads a `Stopped`-before-`Running` by, and why the blob could not
/// say. Only a decodable blob that says `type: simple` selects the shim's vocabulary; an unreadable
/// one reads as strictly as `type: managed`, which is what a shim that cannot decode it reports
/// anyway (a failed start).
fn start_kind(blob: std::result::Result<&Blob, &Arc<Error>>) -> (Kind, Option<Arc<Error>>) {
    match blob {
        Ok(blob) => (blob.spec.kind, None),
        Err(e) => (Kind::Managed, Some(Arc::clone(e))),
    }
}

/// `start`'s error for a `start_via_notify` failure. When the blob was unreadable the start was
/// judged as a `type: managed` service's, and the error says why, since that is not what a
/// `type: simple` service would have been told.
fn failed_start(id: &Id, e: std::io::Error, unreadable: Option<Arc<Error>>) -> Error {
    let failed = Error::os(format!("start `{id}`"), e);
    match unreadable {
        None => failed,
        Some(blob) => Error::StartAsManaged {
            blob,
            source: Box::new(failed),
        },
    }
}

/// Read back everything `ScmRegistration`'s `Display` text needs from the service actually
/// installed at `name`, for the `on_disk` side of `decide`'s comparison —
/// SCM's "artifact" for diffing, since there is no file (see the
/// crate-level design notes on this).
fn read_live_registration(
    cfg: &ServiceConfig,
    service: &Service,
    name: &str,
    live_params: &Parameters,
) -> Result<ScmRegistration> {
    let cmdline = cfg.executable_path.to_string_lossy().into_owned();
    let (executable, arguments) = split_command_line(&cmdline);
    let account = cfg
        .account_name
        .as_ref()
        .map(|a| a.to_string_lossy().into_owned())
        // `ChangeServiceConfigW`'s NULL-means-"unchanged" quirk (see `apply`'s
        // doc comment) forces a live `LocalSystem` account to be written back
        // as some spelling of it, not `None` — canonicalise it through the
        // same table `generate::registration` uses so a `LocalSystem`
        // service (whatever spelling SCM's own readback happens to use)
        // reads as up-to-date rather than a permanent phantom diff. This is
        // the `LocalSystem` row of that same table, folded in rather than a
        // one-off `LocalSystem`-only filter.
        .and_then(|a| generate::canonical_account(&a));
    let failure_actions = read_failure_actions(service, name)?;
    let environment = held_service(name, registry::read_environment(name)?)?;

    Ok(ScmRegistration {
        name: name.to_string(),
        display_name: cfg.display_name.to_string_lossy().into_owned(),
        executable,
        arguments,
        account,
        failure_actions,
        environment,
        parameters: live_params.fields.clone(),
        stray_parameters: live_params.strays.clone(),
    })
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
pub(in crate::backend::scm) fn split_command_line(cmdline: &str) -> (PathBuf, Vec<String>) {
    let wide = identity::wide_null(cmdline);
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

/// Write `reg`: the preconditions first (`SeServiceLogonRight` for a real
/// account, the event source), then the service itself (create or update),
/// then `Parameters`, `Environment` and the failure actions. A precondition
/// that fails fails the install before any service write, leaving no new
/// service and an existing one unchanged, so the next `install` retries as a
/// create or an update, as systemd and launchd do. `Parameters` follows
/// create/update immediately, minimizing the window in which a crash leaves a service with no marker; a failure in
/// the two writes after it leaves a registration the next `install` reads as
/// hand-edited (`Conflict`, cleared by `--force`). An update restates the
/// start type the service has, rather than resetting it; see `service_info`.
fn apply(reg: &ScmRegistration, create: bool) -> Result<()> {
    // Driven by `generate::account_needs_password`, not merely whether
    // `reg.account` is `Some`: a canonicalised built-in
    // (`NT AUTHORITY\LocalService`) or virtual (`NT SERVICE\<id>`) account
    // is `Some`, yet needs no password at all — see that function's own
    // doc comment for the exact table. `LookupAccountNameW`, which the
    // `SeServiceLogonRight` grant goes through, does not resolve
    // `LocalSystem`, `LocalService` or `NetworkService` by those spellings
    // at all, and every such account already holds the right.
    //
    // The `false` arm must never call `identity::service_password()`:
    // `service_password` is a bare read of `GOETIA_SERVICE_PASSWORD` with
    // no knowledge of which account is being installed, so it returns
    // `Some(pw)` whenever that variable happens to be set for any reason —
    // a value left over from installing a different daemon in the same
    // shell or CI job. That stale value has no other diff to catch it
    // (`ScmRegistration` has no password field), so it would otherwise
    // ride silently into `CreateServiceW`/`ChangeServiceConfigW`, which
    // Windows rejects outright for a built-in/virtual account — a live
    // Win32 failure being the only way it would ever surface. "Needs no
    // password" must mean "is never given one", not merely "is not
    // required to have one".
    let password = if generate::account_needs_password(reg.account.as_deref()) {
        let account = reg.account.as_deref().expect(
            "account_needs_password(None) is always false (LocalSystem), so this branch is only reached for Some",
        );
        let pw = identity::service_password()?;
        if pw.is_none() {
            // Install would otherwise report success and the service would
            // fail every future start with error 1069 (no `SeServiceLogonRight`).
            // Refuse instead of creating a service that cannot start.
            return Err(Error::PasswordRequired {
                account: account.to_string(),
            });
        }
        identity::grant_service_logon_right(account)?;
        pw
    } else {
        None
    };

    // Here rather than in the shim: it writes under HKLM, and the shim may run as an unprivileged
    // account.
    registry::register_event_source()?;

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
        let current_start_type = service
            .query_config()
            .map_err(|e| to_error(&format!("query configuration for `{}` to update it", reg.name), e))?
            .start_type;
        let info = service_info(reg, password, Some(current_start_type));
        service
            .change_config(&info)
            .map_err(|e| to_error(&format!("update service `{}`", reg.name), e))?;
    }

    registry::write_parameters(&reg.name, &reg.parameters)?;
    registry::write_environment(&reg.name, &reg.environment)?;
    apply_failure_actions(&reg.name, reg.failure_actions.as_ref())
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
/// (never `SERVICE_DISABLED`, which would block an explicit `start`, nor `SERVICE_AUTO_START`), but an update
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
/// `AdjustTokenPrivileges` beforehand — confirmed empirically.
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
        .map_err(|e| Error::spawn(format!("sc.exe {}", args.join(" ")), e))?;
    if output.status.success() {
        Ok(())
    } else {
        // `sc.exe` reports its errors on stdout.
        Err(Error::command(
            format!("sc.exe {}", args.join(" ")),
            output.status,
            &[output.stdout, output.stderr].concat(),
        ))
    }
}

// uninstall ===========================================================================================================

/// Disable the service, stop whatever runs, then delete it: `DeleteService` on a running service
/// only marks it, and the next `install` meets `ERROR_SERVICE_MARKED_FOR_DELETE`.
///
/// A disabled service is never started again: `StartService` fails with 1058, and a recovery restart
/// fails with event 7032 / `%%1058` and no `START_PENDING`. Measured: a restart queued before the
/// disable, or after it, does not start (P21). A disable that lands before SCM's attempt prevents
/// it. One that lands while the attempt is in flight does not stop it: the attempt runs to
/// `RUNNING`, even for a disable 0.12-0.22 ms after the failure (1 of 8), and the status read right
/// after the disable returns shows `START_PENDING` or `RUNNING` (P23: the status told all 168
/// samples apart), which the settle loop waits out and stops. Once the start type is
/// `SERVICE_DISABLED`, the wait in [`super::wait::settle_and_stop`] sends STOP on a notification. If
/// a start had already begun, the wait lets it finish and then stops it. The service is not marked
/// for deletion until then, so notifications work on it. With the wait actor closed and no other
/// handle open, the delete removes the record at once (P12a). `Budget::Unbounded` throughout:
/// `uninstall` has no `--timeout`, and bounding the stop would turn a slow-stopping service into a
/// failed uninstall where today it succeeds.
///
/// The start type is not put back. A stop or a delete that fails leaves the service disabled. An
/// interrupted `uninstall` (Ctrl-C, a killed goetia) leaves it unchanged, disabled, or deleted,
/// depending on where it stopped. Re-running `uninstall` is the retry in every case. Putting it back could not be kept as a promise: it is not reached when the process
/// dies, and it would overwrite an `enable` or a second `uninstall` that ran in the meantime.
/// Recording the old type on the machine would be new standing state. The error names the previous
/// start type, when it could be read, and the command that sets it again.
///
/// A service someone else already marked for deletion refuses the disable with
/// `ERROR_SERVICE_MARKED_FOR_DELETE` (1072). Then the delete is skipped and a stopped service is
/// success; the record goes with the last handle. A 1072 from the delete itself means the same, and
/// is success too. Both rest on MSDN's description of 1072, not on a probe. A service that is gone
/// (`ERROR_SERVICE_DOES_NOT_EXIST`, 1060) at any of the opens, the disable or the delete is success:
/// the record is removed, which is what `uninstall` is for. A second `uninstall` racing the first
/// can meet 1060 at the first open.
///
/// With `hold`, the service's process is held once it is disabled: just before each STOP, once the
/// state has settled, or at once for a service already `STOP_PENDING`, with no STOP of ours. It is
/// handed back in [`Uninstalled::Deleted`] for the caller to wait for: SCM records `STOPPED`
/// before the process exits, and reports no pid after it. It may be running an older file than the
/// one registered (an `Update` it has not restarted into), which no read of the registry shows. A
/// process that cannot be opened is none (see [`hold_if_possible`]). The service handle each hold
/// uses is closed before it returns, or the service could not be deleted.
fn uninstall_locked(scm: &WinServiceManager, id: &Id, shim: bool, hold: bool) -> Result<Uninstalled> {
    // Opened first and kept through the stop: no state change slips between the disable and the wait.
    let mut actor = match super::wait::SystemScmActor::open(id.as_str()) {
        Ok(actor) => actor,
        Err(e) if is_not_found_io(&e) => return Ok(Uninstalled::Left),
        Err(e) => {
            let refused = open_refused(
                "uninstall",
                &format!("service `{id}`"),
                &format!("open `{id}` before uninstall"),
                windows_service::Error::Winapi(e),
            );
            return Err(uninstall_failed(id, &UninstallFailure::Untouched, refused));
        }
    };

    let (marked, previous) = match disable_before_delete(scm, id)? {
        Disabled::Gone => return Ok(Uninstalled::Left),
        Disabled::AlreadyMarked => (true, None),
        Disabled::Now { previous } => (false, previous),
    };

    // Taken here and not before the disable: a start in flight at the disable, or a recovery
    // restart, replaces the process, and a disabled service cannot be started again. Taken again for
    // each STOP the loop sends, so the one kept is the process that STOP is for, and once for a
    // service that is already `STOP_PENDING`, which no STOP of ours is sent to.
    let mut process = HoldOutcome::NoProcess;
    let mut hold_failed = None;
    let settled = super::wait::settle_and_stop(&mut actor, Budget::Unbounded.start(), shim, |_| {
        if !hold {
            return Ok(());
        }
        let not_done = format!("`{id}`'s process could not be held to wait for its exit");
        match hold_process(id, "uninstall", Budget::Unbounded.start(), &not_done) {
            Ok(HoldOutcome::Expired { .. }) => unreachable!("an unbounded deadline does not expire"),
            Ok(held) => {
                process = held;
                Ok(())
            }
            Err(e) => {
                hold_failed = Some(e);
                Err(std::io::Error::other("the service's process could not be held"))
            }
        }
    });
    match settled {
        Ok(Waited::Confirmed) => {}
        // `let _ = ...` is the write that turns an unreachable case into a silent one, and this
        // is precisely the case where a later edit narrowing that budget must not pass quietly.
        Ok(Waited::Expired) => unreachable!("uninstall's wait is Unbounded; an unbounded deadline never expires"),
        Err(e) => {
            // A stop the shim could not complete is a stopped service, which a re-run reads the same.
            let state = match (is_incomplete_stop(&e), marked) {
                (true, _) => UninstallFailure::DaemonMayRun { previous, marked },
                (false, true) => UninstallFailure::AlreadyMarked,
                (false, false) => UninstallFailure::Stop { previous },
            };
            let cause = hold_failed.unwrap_or_else(|| Error::os(format!("stop `{id}`"), e));
            return Err(uninstall_failed(id, &state, cause));
        }
    }
    // Closed before the delete: an open handle would keep the record after it.
    drop(actor);
    if marked {
        return Ok(Uninstalled::Left);
    }

    let service = match scm.open_service(id.as_str(), ServiceAccess::DELETE) {
        Ok(service) => service,
        Err(e) if is_not_found(&e) => return Ok(Uninstalled::Deleted { process }),
        Err(e) => {
            let refused = open_refused(
                "uninstall",
                &format!("service `{id}`"),
                &format!("open `{id}` to delete it"),
                e,
            );
            return Err(uninstall_failed(id, &UninstallFailure::Delete { previous }, refused));
        }
    };
    match service.delete() {
        Ok(()) => Ok(Uninstalled::Deleted { process }),
        Err(e) if is_not_found(&e) || is_marked_for_delete(&e) => Ok(Uninstalled::Deleted { process }),
        Err(e) => Err(uninstall_failed(
            id,
            &UninstallFailure::Delete { previous },
            to_error(&format!("delete service `{id}`"), e),
        )),
    }
}

/// How far a successful [`uninstall_locked`] got.
enum Uninstalled {
    /// The service was stopped and is deleted, or is about to be when a handle closes: nothing it
    /// ran is running or will start again, once the process SCM reported has exited. `process` is
    /// what holding it came to: never an expiry, since the hold has no bound.
    Deleted { process: HoldOutcome<Held> },
    /// Nothing of this call's is left to wait for: the service was already gone (1060), or someone
    /// else had marked it for deletion, so it was stopped but not deleted by this call. What it ran
    /// is left for the next sweep.
    Left,
}

// A shim host ---------------------------------------------------------------------------------------------------------

/// The host `snapshot`'s service is registered with, when its file name is that of a shim file
/// placed by a provider (`goetia-shim-<hex>.exe`): the one case whose file an uninstall may sweep.
/// `None` for another host, and for a service already marked for deletion or gone, which refuses the
/// read as a matter of course (`uninstall_locked` still finishes it). Any other failure to read the
/// configuration is an error: the host is not known, which is not "no shim".
fn registered_shim(snapshot: &ScmSnapshot) -> Result<Option<PathBuf>> {
    match snapshot.config() {
        Ok(config) => Ok(shim_host(&config.executable_path.to_string_lossy())),
        Err(e) if is_gone_or_marked(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether the service's process may be a goetia shim, whose records the shim's vocabulary reads
/// (an incomplete stop fails a `stop`): its registered image is a shim host, whatever its blob says, so
/// a blob that will not decode over a shim host is still judged and a `type: managed` exe never is. A
/// configuration that cannot be read leaves the host unknown, and unknown counts as a shim: a refusal
/// to call a stop complete costs a re-run, and a tree left running costs a second copy beside it.
fn runs_a_shim(snapshot: &ScmSnapshot) -> bool {
    let image = snapshot
        .config()
        .ok()
        .map(|config| config.executable_path.to_string_lossy().into_owned());
    is_shim_image(image.as_deref())
}

/// [`runs_a_shim`] over the registered command line, `None` when it could not be read.
fn is_shim_image(image: Option<&str>) -> bool {
    image.is_none_or(|image| shim_host(image).is_some())
}

/// The host the command line `image` starts, when its file name is that of a shim file placed by a
/// provider.
fn shim_host(image: &str) -> Option<PathBuf> {
    let (exe, _) = split_command_line(image);
    let is_shim = exe.file_name()?.to_str().and_then(shim_file) == Some(ShimFile::Current);
    is_shim.then_some(exe)
}

/// Whether `error`, from reading a service's configuration, says the service is gone or marked for
/// deletion.
fn is_gone_or_marked(error: &Error) -> bool {
    matches!(error, Error::NotInstalled { .. })
        || matches!(
            error.raw_os_error(),
            Some(code) if code == ERROR_SERVICE_MARKED_FOR_DELETE as i32 || code == ERROR_SERVICE_DOES_NOT_EXIST as i32
        )
}

/// The process SCM reports for `id`, held before the service is stopped: SCM records `STOPPED`
/// before the process exits, and reports no pid after it. For an uninstall it may be running an
/// older file than the one registered (an `Update` it has not restarted into), which no read of the
/// registry shows. [`HoldOutcome::NoProcess`] when SCM reports none, and [`HoldOutcome::Refused`],
/// with why, when the process cannot be opened (see [`hold_if_possible`]): the caller goes on, and
/// what that process runs stays referenced by its image until it exits. A failure to read SCM is an
/// error, led by `not_done`, what the caller did not do. The wait for SCM to let go of a pid whose
/// process is gone ends at `deadline`, and is [`HoldOutcome::Expired`]: a `stop` sends its STOP all
/// the same. A service gone since it was read has none. The service handle the hold used is closed
/// before this returns, or the service could not be deleted. `operation` is the verb the hold is
/// for, which [`open_existing`] names in a denial.
fn hold_process(id: &Id, operation: &str, deadline: Deadline, not_done: &str) -> Result<HoldOutcome<Held>> {
    let not_done = |e: Error| Error::Step {
        what: not_done.to_string(),
        source: Box::new(e),
    };
    let (_scm, service) = match open_existing(id, operation, ServiceAccess::QUERY_STATUS) {
        Ok(opened) => opened,
        Err(Error::NotInstalled { .. }) => return Ok(HoldOutcome::NoProcess),
        Err(e) => return Err(not_done(e)),
    };
    let mut target = super::kill::SystemKillTarget::new(service, id.as_str().to_string());
    hold_if_possible(&mut target, deadline)
        .map_err(|e| not_done(Error::os(format!("hold `{id}`'s process to wait for its exit"), e)))
}

/// The end of a process held before `id`'s STOP, once SCM has confirmed `STOPPED`: [`end_process`]
/// over the real process, for `stop` and `uninstall` alike. A wait with no end says on stderr why it
/// is blocked; a bounded one is the usual case (the shim exits just after it reports `STOPPED`) and
/// says nothing.
///
/// The process is ended with exit code 1, which can reach it before the shim's own exit and replace
/// the code the shim ends itself with; SCM's record is unaffected.
fn end_held(id: &Id, held: Held, deadline: Deadline) -> std::io::Result<EndProcessOutcome<Held>> {
    end_process(held, deadline, Held::terminate, |held, deadline, terminate_failed| {
        held.wait_for_exit(deadline, || {
            if deadline.at().is_none() {
                held.notice(id.as_str(), terminate_failed);
            }
        })
    })
}

/// `error`, the failure of a `stop` whose process SCM vouched for and goetia could not open, for
/// `reason`: led by what that left undone, with the class of `error`.
fn unwaited(id: &Id, reason: std::io::Error, error: Error) -> Error {
    Error::Step {
        what: format!(
            "goetia could not open `{id}`'s process ({}), so it could not wait for the process to exit",
            Chain(&reason)
        ),
        source: Box::new(error),
    }
}

/// The error of a `stop` whose hold expired and whose STOP was sent with nothing held: the
/// [`WaitTimeout`](Error::WaitTimeout) of a stop SCM did not confirm, led by what makes it so.
fn unconfirmed_stop(id: &Id, pid: u32, budget: Budget) -> Error {
    Error::Step {
        what: format!(
            "goetia could not hold `{id}`'s process before the time ran out (SCM still named process {pid}, which \
             was gone), so it cannot confirm that the service's process exited, whether or not SCM reports the \
             service stopped"
        ),
        source: Box::new(budget::timed_out(id.as_str(), "stopped", budget)),
    }
}

impl Scm {
    /// The folder of the provider, as an uninstall tells it: [`ProviderFolder::None`] without a
    /// provider or for one that sweeps no folder, its normalised `folder()`, and
    /// [`ProviderFolder::Unknown`] when that fails, which is not the absence of a provider: the
    /// failure comes with it, for the uninstall to report.
    fn provider_folder(&self) -> (ProviderFolder, Option<Error>) {
        let Some(shim) = &self.shim else {
            return (ProviderFolder::None, None);
        };
        match shim.folder() {
            Ok(Some(folder)) => (ProviderFolder::At(normalise(&folder)), None),
            Ok(None) => (ProviderFolder::None, None),
            Err(e) => (
                ProviderFolder::Unknown,
                Some(Error::Step {
                    what: "the folder of this program's shim provider could not be determined".to_string(),
                    source: Box::new(e),
                }),
            ),
        }
    }

    /// The error of an uninstall that deleted `id` but did not clean up its shim files, because the
    /// read of its registered host failed with `unread`: the host is not known, so nothing was
    /// waited for or swept. Told as [`finish_uninstall`](Self::finish_uninstall) tells a cleanup that
    /// failed, with the retry that applies.
    fn cleanup_skipped(&self, id: &Id, unread: Error) -> Error {
        let (provider, unknown) = self.provider_folder();
        let skipped = Error::Step {
            what: format!("`{id}`'s host could not be read, so no shim file of it was waited for or swept"),
            source: Box::new(unread),
        };
        let cause = match unknown {
            Some(unknown) => Error::Both {
                first: Box::new(skipped),
                then: Box::new(unknown),
            },
            None => skipped,
        };
        Error::Step {
            what: left_behind_text(id, &provider, None, &[]),
            source: Box::new(cause),
        }
    }

    /// What an uninstall does once `id` is stopped and deleted, for a service whose host was the shim
    /// file `host`: ends the process that was held, as the documentation of `SetServiceStatus`
    /// allows once `SERVICE_STOPPED` is reported, and waits, with no bound, for it to exit (SCM
    /// recorded `STOPPED` before it exits, and a `stop` that waited has left none to hold), and for
    /// no other: a process it did not hold cannot be tied to the service it is uninstalling, and
    /// waiting for another service's stuck process would hold the install lock. Then sweeps the
    /// provider's folder and, when `host` was in the default folder and that is another, that one
    /// too (see [`second_folder`]): the service may have been installed by another goetia version or
    /// provider. A host in any other folder is not swept, since that folder is not goetia's.
    ///
    /// A default folder that cannot be located skips the second folder, as it cannot be told from the
    /// host's, and is reported with whatever else fails: the wait and the provider's sweep go on.
    /// A provider's sweep that fails on the same lookup reports it too: twice in all.
    ///
    /// A provider whose `folder()` failed has no folder to name, and nothing is promised retried. The
    /// stock providers fail their sweep on the same cause, which is then reported once; for another
    /// provider it is told beside a failure of the cleanup, see [`finished_cleanup`].
    ///
    /// A failure here leaves the service deleted, and says so, naming each file that remains and
    /// what removes it: see [`left_behind_text`]. A file kept because a service names it or a process
    /// runs it is in use, and is skipped without an error: the next sweep removes it once it is free.
    fn finish_uninstall(&self, id: &Id, host: &Path, process: HoldOutcome<Held>) -> Result<()> {
        let host = normalise(host);
        let (provider, unknown) = self.provider_folder();
        // A second folder is the host's folder only when it is the default, and that is what its
        // sweep removes it by. The provider asks where the default folder is too, so a lookup that
        // fails can be reported twice. When it cannot be located the host's folder may be it, so it
        // is named, not swept, and nothing else is skipped.
        let mut failures: Vec<SweepFailure> = Vec::new();
        let (second, unswept) = match host
            .parent()
            .map(|folder| second_folder(folder, provider.path(), || Ok(normalise(&folder_in(None)?))))
            .transpose()
        {
            Ok(second) => (second.flatten(), second.flatten()),
            Err(e) => {
                failures.push(e.into());
                (None, host.parent())
            }
        };
        let finish = |failures: Vec<SweepFailure>, provider_failed: bool| {
            finished_cleanup(id, &provider, unswept, unknown, provider_failed, failures)
        };
        match process {
            // `uninstall_locked` returned only after SCM recorded `STOPPED`, so the process may be
            // ended, through the handle held since before the stop, and waited for as `stop` waits
            // for it (see [`end_held`]), with no bound: the shim exits by itself after it reports
            // `STOPPED`, and the terminate is a backstop.
            HoldOutcome::Held(held) => match end_held(id, held, Budget::Unbounded.start()) {
                Ok(EndProcessOutcome::Exited) => {}
                Ok(EndProcessOutcome::Lingers { .. }) => unreachable!("an unbounded deadline never expires"),
                Err(e) => {
                    failures.push(Error::os(format!("wait for `{id}`'s process to exit"), e).into());
                    return finish(failures, false);
                }
            },
            HoldOutcome::NoProcess => {}
            // Not waited for, and said so: the file it runs stays until it exits, and the sweep below
            // keeps it by its image.
            HoldOutcome::Refused(reason) => {
                failures.push(Error::os(format!("open `{id}`'s process to wait for its exit"), reason).into())
            }
            HoldOutcome::Expired { .. } => unreachable!("an unbounded deadline never expires"),
        }
        // After the wait: what has exited no longer references its file.
        let referenced = match referenced_hosts() {
            Ok(referenced) => referenced,
            Err(e) => {
                failures.push(e.into());
                return finish(failures, false);
            }
        };
        let mut provider_failed = false;
        if let Some(shim) = &self.shim {
            let swept = shim.sweep(&referenced, &self.folders).err();
            provider_failed = swept.is_some();
            failures.extend(swept);
        }
        if let Some(folder) = second {
            // It is the default folder, by `second_folder`'s choice.
            failures.extend(sweep_folder(folder, &referenced, &self.folders, || Ok(folder.to_path_buf())).err());
        }
        finish(failures, provider_failed)
    }
}

/// The result of the cleanup of an uninstall that deleted `id`: `Ok` when every sweep succeeded, and
/// otherwise the failures, led by what remains (see [`left_behind_text`]). `unknown` is why the
/// provider's folder could not be named: it is no failure of its own, and is told as context of one
/// that is. Not when `provider_failed`, the provider's own sweep having failed: the stock providers
/// fail it on that same cause, which is then already among `failures`.
fn finished_cleanup(
    id: &Id,
    provider: &ProviderFolder,
    unswept: Option<&Path>,
    unknown: Option<Error>,
    provider_failed: bool,
    failures: Vec<SweepFailure>,
) -> Result<()> {
    SweepFailure::all(failures).map_or(Ok(()), |failure| {
        let files: Vec<PathBuf> = failure.not_deleted.iter().map(|file| normalise(file)).collect();
        let cause = match unknown.filter(|_| !provider_failed) {
            Some(unknown) => Box::new(Error::Both {
                first: failure.error,
                then: Box::new(unknown),
            }),
            None => failure.error,
        };
        Err(Error::Step {
            what: left_behind_text(id, provider, unswept, &files),
            source: cause,
        })
    })
}

/// What [`disable_before_delete`] did.
enum Disabled {
    /// The start type is now `SERVICE_DISABLED`. `previous` is what it was, or `None` when reading
    /// it failed, which does not stop the uninstall.
    Now { previous: Option<ServiceStartType> },
    /// Someone else already marked the service for deletion (1072): nothing was changed.
    AlreadyMarked,
    /// The service no longer exists (1060).
    Gone,
}

/// Read `id`'s start type, then set it to `SERVICE_DISABLED`. A service someone else marked for
/// deletion (1072) is [`Disabled::AlreadyMarked`] and one that is gone (1060) is [`Disabled::Gone`];
/// neither is an error. A start type that cannot be read is not an error either: only the message
/// that names it needs it, so it is left out.
fn disable_before_delete(scm: &WinServiceManager, id: &Id) -> Result<Disabled> {
    let service = match scm.open_service(id.as_str(), ServiceAccess::CHANGE_CONFIG | ServiceAccess::QUERY_CONFIG) {
        Ok(service) => service,
        Err(e) if is_not_found(&e) => return Ok(Disabled::Gone),
        Err(e) => {
            let refused = open_refused(
                "uninstall",
                &format!("service `{id}`"),
                &format!("open `{id}` to disable it"),
                e,
            );
            return Err(uninstall_failed(id, &UninstallFailure::Untouched, refused));
        }
    };
    let previous = match service.query_config() {
        Ok(cfg) => Some(cfg.start_type),
        Err(e) if is_marked_for_delete(&e) => return Ok(Disabled::AlreadyMarked),
        Err(_) => None,
    };
    match set_start_type(&service, ServiceStartType::Disabled) {
        Ok(()) => Ok(Disabled::Now { previous }),
        Err(e) if is_marked_io(&e) => Ok(Disabled::AlreadyMarked),
        Err(e) if is_not_found_io(&e) => Ok(Disabled::Gone),
        // `ChangeServiceConfigW` is not documented to leave the configuration as it was when it
        // fails, so this claims only that the delete did not happen.
        Err(e) => Err(uninstall_failed(
            id,
            &UninstallFailure::DisableFailed,
            Error::os(format!("disable `{id}` before uninstall"), e),
        )),
    }
}

/// How a failed `uninstall` left the service, which decides what [`uninstall_failed`] may say.
enum UninstallFailure {
    /// It failed before it changed anything: the service is as it was.
    Untouched,
    /// The disable that precedes the delete failed. Whether it changed the start type is unknown.
    DisableFailed,
    /// The stop failed. The service is running and disabled; `previous` is its start type before.
    Stop { previous: Option<ServiceStartType> },
    /// The delete failed. The service is stopped and disabled; `previous` is its start type before.
    Delete { previous: Option<ServiceStartType> },
    /// Someone else had marked the service for deletion, so `uninstall` did not delete it. Its wait
    /// may already have sent a STOP before it failed.
    AlreadyMarked,
    /// The service is stopped, and SCM recorded that the shim could not end the daemon's process tree.
    /// Unless `marked` (someone else had marked it for deletion) it is disabled, with `previous` its
    /// start type before, and was not deleted.
    DaemonMayRun {
        previous: Option<ServiceStartType>,
        marked: bool,
    },
}

/// An `uninstall` that failed with `cause`, led by what it left behind. Every state names the retry
/// or the remedy that applies to it, and none claims a state it is not in.
fn uninstall_failed(id: &Id, state: &UninstallFailure, cause: Error) -> Error {
    let kill = format!("if the service does not accept STOP, run `goetia daemon kill {id}`");
    let what = match state {
        UninstallFailure::Untouched => format!(
            "`{id}` was NOT deleted and nothing about it was changed. Re-run \
             `goetia daemon uninstall {id}` to retry"
        ),
        UninstallFailure::DisableFailed => format!(
            "`{id}` was NOT deleted, and the disable that precedes the delete failed, so its start type \
             may have changed. Re-run `goetia daemon uninstall {id}` to retry"
        ),
        UninstallFailure::Stop { previous } => format!(
            "`{id}` could not be stopped. It was NOT deleted and is now disabled, so \
             `goetia daemon start {id}` fails.{} Re-run `goetia daemon uninstall {id}` to retry; {kill} first",
            previous_start_type(id, *previous)
        ),
        UninstallFailure::Delete { previous } => format!(
            "`{id}` is stopped and disabled but was NOT deleted.{} Re-run \
             `goetia daemon uninstall {id}` to retry",
            previous_start_type(id, *previous)
        ),
        UninstallFailure::AlreadyMarked => format!(
            "`{id}` could not be stopped. It was already marked for deletion by someone else, \
             so `uninstall` did not delete it. It is removed once it stops and its last handle closes; \
             {kill}"
        ),
        UninstallFailure::DaemonMayRun { previous, marked } => {
            let left = if *marked {
                "It was already marked for deletion by someone else, so `uninstall` did not delete it; \
                 it is removed once its last handle closes."
                    .to_string()
            } else {
                format!(
                    "It was NOT deleted and is now disabled, so `goetia daemon start {id}` fails.{} \
                     `uninstall` fails the same while that record stands and has no option that deletes over \
                     it. If you know the process tree is gone, `sc.exe delete {id}` removes the service.",
                    previous_start_type(id, *previous)
                )
            };
            format!(
                "`{id}` is stopped, but SCM recorded that goetia-shim could not end the daemon's process \
                 tree, so the daemon may still be running. {left}"
            )
        }
    };
    Error::Step {
        what,
        source: Box::new(cause),
    }
}

/// The sentence naming the start type `uninstall` found and the command that sets it again, with its
/// leading space; empty when the type could not be read. A `Disabled` found is not the original: an
/// earlier `uninstall` probably set it, and nothing records what it replaced.
fn previous_start_type(id: &Id, previous: Option<ServiceStartType>) -> String {
    let (name, command) = match previous {
        None => return String::new(),
        Some(ServiceStartType::Disabled) => {
            return format!(
                " Its original start type is unknown, because an earlier `uninstall` probably disabled it; \
                 `goetia daemon enable {id}` or `goetia daemon disable {id}` sets one."
            );
        }
        Some(ServiceStartType::AutoStart) => ("automatic", format!("`goetia daemon enable {id}`")),
        Some(ServiceStartType::OnDemand) => ("manual", format!("`goetia daemon disable {id}`")),
        Some(ServiceStartType::SystemStart) => ("system", format!("`sc.exe config {id} start= system`")),
        Some(ServiceStartType::BootStart) => ("boot", format!("`sc.exe config {id} start= boot`")),
    };
    format!(" Its start type was {name}; {command} sets it again.")
}

// shared helpers ======================================================================================================

pub(in crate::backend::scm) fn open_scm(access: ServiceManagerAccess) -> Result<WinServiceManager> {
    WinServiceManager::local_computer(None::<&str>, access).map_err(|e| to_error("open the Service Control Manager", e))
}

/// [`open_scm`] for the caller that is about to *classify* `id`, [`Backend::read`]. A manager that
/// would not open leaves that id exactly as unclassified as a service object that would not open
/// does, so it takes the same class: an `Os` error here would reach `status_error`'s catch-all as
/// `Kind::Unusable`, which claims ownership. Callers that open after ownership is settled use
/// [`open_scm_for`] or [`open_scm`].
///
/// Deliberately untested, and stated rather than contrived: `SC_MANAGER_CONNECT` is granted to every
/// token on a working host, and the only way to withhold it is to rewrite the SCM database's own
/// security descriptor — which stops `services.msc`, the test harness's own `ServiceGuard` cleanup
/// and every other service operation on the runner, and is not restorable by a `Drop` that may not
/// run. What is covered instead is the mapping this line performs, in full: the same
/// [`service_undetermined`] over the same [`to_error`] cause, at
/// `manager_tests.rs::the_scm_undetermined_recovery_names_both_causes_and_not_uninstall` for both
/// causes, and end to end against a real denial at
/// `tests/scm_integration/managed.rs::status_of_a_service_whose_object_denies_querying_is_undetermined`,
/// which reaches it through `read`'s service-object open instead.
fn open_scm_to_classify(id: &str) -> Result<WinServiceManager> {
    WinServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| service_undetermined(id, "open the Service Control Manager", e))
}

/// The `ImagePath` of the service `name`, read through `scm` (`SERVICE_QUERY_CONFIG`) rather than its
/// registry key, which may deny reads that SCM does not. `None` when the service is gone or marked
/// for deletion, which start nothing.
pub(in crate::backend::scm) fn read_image_through_scm(scm: &WinServiceManager, name: &str) -> Result<Option<String>> {
    let service = match scm.open_service(name, ServiceAccess::QUERY_CONFIG) {
        Ok(service) => service,
        Err(e) if is_not_found(&e) => return Ok(None),
        Err(e) => return Err(to_error(&format!("open service `{name}` to read its image path"), e)),
    };
    match service.query_config() {
        Ok(config) => Ok(Some(config.executable_path.to_string_lossy().into_owned())),
        Err(e) if is_marked_for_delete(&e) => Ok(None),
        Err(e) => Err(to_error(&format!("query configuration for `{name}`"), e)),
    }
}

fn is_not_found(e: &windows_service::Error) -> bool {
    matches!(e, windows_service::Error::Winapi(io) if is_not_found_io(io))
}

fn is_not_found_io(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32)
}

/// Whether `e` is `ERROR_SERVICE_MARKED_FOR_DELETE` (1072).
fn is_marked_for_delete(e: &windows_service::Error) -> bool {
    matches!(e, windows_service::Error::Winapi(io) if is_marked_io(io))
}

fn is_marked_io(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(ERROR_SERVICE_MARKED_FOR_DELETE as i32)
}

/// Whether `e` is `ERROR_ACCESS_DENIED` — the one thing the error code decides: which of
/// [`undetermined`]'s two `recovery` texts applies, and whether [`open_refused`] names a denial.
fn is_access_denied(e: &windows_service::Error) -> bool {
    matches!(
        e,
        windows_service::Error::Winapi(io) if io.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
    )
}

/// A `windows_service` failure as an [`Error::Os`], shared with [`service_undetermined`] so one
/// failure cannot be described two ways depending on which class it lands in.
///
/// `windows_service::Error`'s own `Display` for its `Winapi` variant is the
/// literal string `"IO error in winapi call"` — it does not include the
/// wrapped `io::Error`'s message at all (verified: every error surfaced
/// through `to_error` read that and nothing else, useless for diagnosing an
/// actual Win32 failure). Unwrap to the inner `io::Error` first, whose
/// `Display` does carry `FormatMessage`'s real text.
fn to_error(context: &str, e: windows_service::Error) -> Error {
    match e {
        windows_service::Error::Winapi(io_err) => Error::os(context, io_err),
        other => Error::os(context, other),
    }
}

// undetermined ========================================================================================================

/// This backend's single [`Error::Undetermined`] constructor — the SCM twin of
/// `backend::systemd::manager::discover::undetermined` and
/// `backend::launchd::manager::undetermined`, worded to match them, differing
/// only where the platform names its own privilege boundary.
///
/// Neither read that reaches it is a file, and the two are not even the same
/// kind of object: one is the `Services\<id>\Parameters` registry key
/// ([`registry::read_parameters`]), the other an SCM service object
/// (`read`'s own open of it). Only the caller knows which, so `reason`
/// arrives already built, as the `Error::Os` [`to_error`] or `registry`'s `registry_error` makes,
/// and this decides the class and the remedy.
///
/// [`Error::Undetermined`] over that `Os`, never the `Os` alone: `Os` reaches
/// `cli::report::status_error`'s catch-all as `Kind::Unusable` (see [`Error::Undetermined`]).
/// `HKLM\SYSTEM\CurrentControlSet\Services` holds
/// every vendor's service, so on this platform that claim is routinely about a
/// stranger's, and it publishes `uninstall` as the remedy for one.
///
/// Every failure but "no such service"/"no such key" lands here, not
/// `ERROR_ACCESS_DENIED` alone: a hive that will not read leaves goetia exactly
/// as ignorant of the id as a denial does, so a class chosen by error code
/// would restore the false ownership claim for the narrower input while the fix
/// looked complete. What `access_denied` does choose is `recovery` — elevation
/// is advice only a permission boundary earns, and offering it for a failing
/// disk sends the reader somewhere useless.
fn undetermined(id: &str, reason: Error, access_denied: bool) -> Error {
    let recovery = if access_denied {
        "re-run as Administrator: that read is what tells goetia whether anything is installed at \
         this id"
    } else {
        "resolve that failure and re-run: that read is what tells goetia whether anything is \
         installed at this id"
    };
    Error::Undetermined {
        id: id.to_string(),
        reason: Box::new(reason),
        recovery: recovery.to_string(),
    }
}

/// As above, for reads that establish nothing (before ownership is known).
fn service_undetermined(id: &str, context: &str, e: windows_service::Error) -> Error {
    let access_denied = is_access_denied(&e);
    undetermined(id, to_error(context, e), access_denied)
}

/// A failed open made after `read` established that the id is installed and goetia's, so the verb
/// `operation` failed and nothing is in doubt: an `Os` error (exit `1`), never [`undetermined`],
/// which says goetia cannot tell whether anything is installed. A denial's message names the verb
/// and the `target` it was denied.
///
/// `context` is the open, as [`to_error`] takes it.
fn open_refused(operation: &str, target: &str, context: &str, e: windows_service::Error) -> Error {
    let access_denied = is_access_denied(&e);
    let failed = to_error(context, e);
    if !access_denied {
        return failed;
    }
    Error::Step {
        what: format!("`{operation}` was denied access to {target}"),
        source: Box::new(failed),
    }
}

/// [`open_scm`] for the verbs that act on an id `read` has already established as goetia's.
/// [`open_refused`] words its failure; [`open_scm_to_classify`] is for `read` alone.
fn open_scm_for(id: &Id, operation: &str) -> Result<WinServiceManager> {
    WinServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(|e| {
        open_refused(
            operation,
            "the service manager",
            &format!("open the Service Control Manager to {operation} `{id}`"),
            e,
        )
    })
}

/// Open `id` with the `access` the verb `operation` needs beyond what `read` asked for, for a caller
/// that holds the snapshot `read` made: the id is installed and goetia's. "No such service" is
/// [`Error::NotInstalled`], not a raw Win32 message; every other failure is [`open_refused`].
fn open_existing(id: &Id, operation: &str, access: ServiceAccess) -> Result<(WinServiceManager, Service)> {
    let scm = open_scm_for(id, operation)?;
    let service = scm.open_service(id.as_str(), access).map_err(|e| {
        if is_not_found(&e) {
            Error::NotInstalled {
                id: id.as_str().to_string(),
            }
        } else {
            open_refused(
                operation,
                &format!("service `{id}`"),
                &format!("open service `{id}` to {operation} it"),
                e,
            )
        }
    })?;
    Ok((scm, service))
}

fn map_state(s: WinState) -> State {
    match s {
        WinState::Running => State::Running,
        // SCM has no state distinct from `Stopped` for "stopped because it
        // failed" — `dwWin32ExitCode`/`dwServiceSpecificExitCode` carry that,
        // not `dwCurrentState`. Recovery actions may already have restarted
        // it by the time this is read, in any case.
        WinState::Stopped => State::Stopped,
        // One of three places a `State::Starting` would be produced if
        // goetia grows one (routed post-0.1.0) — the other two are
        // `state::classify` in `src/backend/launchd/state.rs` and
        // `status_from_unit` in `src/backend/systemd/manager/systemctl.rs`.
        // This match is exhaustive over `WinState`, so adding that variant
        // to `State` will not silently leave this arm behind.
        WinState::StartPending
        | WinState::StopPending
        | WinState::ContinuePending
        | WinState::PausePending
        | WinState::Paused => State::Unknown,
    }
}

fn query_live(service: &Service, name: &str) -> Result<(State, Option<u32>)> {
    let status = service
        .query_status()
        .map_err(|e| to_error(&format!("query status for `{name}`"), e))?;
    Ok((map_state(status.current_state), status.process_id))
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

/// The `list` entry standing for every service goetia could not classify, and
/// `None` when there were none — an ordinary listing must carry nothing under
/// the third key rather than exit `4` forever.
///
/// **One entry, not one per service.** Most services on a Windows box carry an
/// ACL that denies a non-elevated read of their `Parameters`, so an entry each
/// would bury `goetia daemon list` under hundreds of them, about services that
/// were never goetia's. Nor is there a sound pre-filter: a `type: managed`
/// service's `ImagePath` is the user's own binary, so the marker under
/// `Parameters` is the only thing that tells goetia's apart — and it is the
/// unread thing. Aggregating belongs in the backend because only the backend
/// knows the denominator.
///
/// **A single name is still carried.** Dropping the name is what one entry
/// standing for hundreds costs; it costs nothing when the entry stands for one,
/// and [`Installed::Undetermined`] states the invariant directly — *an entry for
/// exactly one known id always names it*. The difference is not cosmetic. A
/// null name forbids every negative conclusion about **every** id on the host,
/// so one Defender-protected `Parameters` key would make
/// `goetia daemon show <anything-not-installed>` answer "could not be
/// determined" (exit `4`) instead of "not installed" (exit `1`), permanently,
/// for ids that have nothing to do with it. Naming the one service it stands
/// for confines that to the one id it is actually true of. Its cause travels with it, and so does
/// the remedy that cause earned, for the same reason and at the same cost — see
/// [`named_unreadable_notice`].
fn unreadable_aggregate(unreadable: Vec<Unreadable>) -> Option<Installed> {
    let mut unreadable = unreadable.into_iter();
    let (first, count) = (unreadable.next()?, 1 + unreadable.count());
    // Exactly one, so the entry can be about it rather than about the host — and so must its text.
    // The name and the reason are one report: an entry that names its service and then says a
    // daemon may be missing from the list contradicts itself in a single rendered line.
    Some(match count {
        1 => Installed::Undetermined {
            name: Some(first.name),
            reason: named_unreadable_notice(first.detail, first.recovery.as_deref()),
        },
        _ => Installed::Undetermined {
            name: None,
            reason: Reason::said(unreadable_notice(count)),
        },
    })
}

/// One service `list` reached and could not read, kept whole because
/// [`unreadable_aggregate`] cannot recover any of it afterwards: a count knows
/// neither the name, nor the cause, nor which remedy that cause earned.
struct Unreadable {
    name: String,
    /// [`registry::read_parameters`]'s own error for what failed: the key,
    /// the operation and the OS error.
    detail: Error,
    /// The remedy [`undetermined`] chose from that failure's errno, or `None`
    /// for one that arrived without a conditioned remedy to carry.
    recovery: Option<String>,
}

/// Split the failure [`registry::read_parameters`] returned into the parts an entry standing for
/// that one read can carry, dropping only the variant's `id` — which `cli::support` renders itself,
/// from the name, before this text.
///
/// The remedy travels for the reason the cause does: at a count of one this entry and
/// `status <same-id>` report the same single failed read, so answering it twice, differently, would
/// put two answers on one condition. `read_parameters` builds every failure it has through
/// `registry_undetermined`, so the first arm is the whole story; the second is for a `Result` type
/// that does not say so, and has no conditioned remedy to offer rather than a default one.
fn unreadable_parts(name: String, e: Error) -> Unreadable {
    match e.into_undetermined() {
        Ok((reason, recovery)) => Unreadable {
            name,
            detail: *reason,
            recovery: Some(recovery),
        },
        Err(e) => {
            debug_assert!(false, "`read_parameters` fails only as Undetermined, got {e:?}");
            Unreadable {
                name,
                detail: e,
                recovery: None,
            }
        }
    }
}

/// The text an entry that *names* its one service carries, rendered by `cli::support` after that
/// name: `warning: MsSecFlt: installation state could not be determined: <this>`.
///
/// `detail` is [`registry::read_parameters`]'s own rendering of what failed, carried through rather
/// than replaced by a static sentence: it names the key, the operation and the Win32 error, which is
/// the only part of this that can distinguish a denied read from a corrupt hive. A count of more
/// than one has a reason to drop it ([`unreadable_notice`] — hundreds of causes, one entry); a count
/// of one has none.
///
/// Nothing here is missing from the list, and saying so is the whole difference the name makes —
/// what is unknown is whether the service goetia just named is one of its own.
///
/// `recovery` is [`undetermined`]'s own choice, carried rather than re-worded: that function
/// conditions the remedy on whether the failure was a denial precisely because "elevation is advice
/// only a permission boundary earns", and an entry standing for one read has the same errno in hand
/// that `status` does. Offering elevation unconditionally here told an already-elevated host to
/// re-run elevated over a corrupt hive, while `status <same-id>` told it to fix the hive.
/// [`unreadable_notice`]'s hedge is reasoning about a *count* — hundreds of causes, one entry — and
/// does not reach a count of one.
fn named_unreadable_notice(detail: Error, recovery: Option<&str>) -> Reason {
    let after = match recovery {
        Some(recovery) => format!(" — so whether this service is one of goetia's is unknown; {recovery}."),
        None => " — so whether this service is one of goetia's is unknown.".to_string(),
    };
    Reason::around("", detail, after)
}

/// The text [`unreadable_aggregate`]'s *unnamed* entry carries: how many
/// services could not be inspected, that ownership is therefore *unknown*,
/// and the remedy. An entry standing for exactly one names it and carries
/// [`named_unreadable_notice`] instead, so nothing here has to hold for a
/// count of one.
///
/// It deliberately does **not** say *why* the read failed. `list` counts
/// every non-`NotFound` failure here, and access denial is only the common
/// one — a corrupt hive or an `ERROR_IO_DEVICE` lands in the same count.
/// Naming denial as the cause would assert something this never
/// established and hand over a remedy that cannot work for the others,
/// which is exactly what the per-id path avoids by carrying two separate
/// recoveries. Elevation is offered as the usual remedy, not as the
/// diagnosis.
fn unreadable_notice(count: usize) -> String {
    debug_assert!(count > 1, "one service is named, not counted: see unreadable_aggregate");
    format!(
        "{count} services could not be inspected (their registry Parameters could not be read). Ownership is unknown for them, so a Goetia daemon may be missing from this list; on an unelevated run, re-running elevated is the usual remedy."
    )
}

//! The Windows service host `type: simple` daemons run under (`goetia-shim`).
//!
//! SCM has no working-directory, environment, or stdout-capture field of its
//! own (`backend::scm::generate`'s mapping table calls these cells "shim
//! spawns child"/"shim sets it"/"shim writes"), so for `type: simple`
//! `ScmRegistration::executable` always names the shim binary — never the
//! daemon's own command. [`run`] takes the service name SCM hands `ServiceMain`,
//! reads that daemon's real `DaemonSpec` back out of the same
//! `Services\<name>\Parameters` metadata blob the SCM backend wrote it into, and
//! does everything a real service host needs: spawn the daemon once, capture
//! its output, report its exit as the service's exit, and kill its whole
//! process tree on `SERVICE_STOP`.

mod exit;
mod logging;
mod os;
mod service;
mod stop_bus;
#[cfg(test)]
mod test_support;

pub(crate) use service::run;

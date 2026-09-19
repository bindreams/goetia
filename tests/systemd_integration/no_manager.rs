//! Where no systemd manager can be asked — systemd's offline mode, a chroot, a system systemd did
//! not boot — every verb that reaches the manager refuses, exit `1`, before it writes or sends
//! anything, with one message naming what showed it. The verbs that never reach the manager are
//! untouched.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use goetia::backend::systemd::manager::Systemd;
use goetia::manager::{Budget, ServiceManager};
use goetia::spec::Id;

use crate::linux::{active_state_and_job, main_pid, mk, unit_path, wants_symlink, world_readable_manifest};
use crate::support::{self, ELEVATED, ServiceGuard};

/// How a host shows that no systemd manager can be asked, each the way goetia must find it out.
#[derive(Debug, Clone, Copy)]
enum NoManager {
    /// `SYSTEMD_OFFLINE=1`, systemd's own offline switch, which goetia reads itself.
    Offline,
    /// `SYSTEMD_IN_CHROOT=1`: `systemctl` behaves as in a chroot, and only its own report says so.
    Chroot,
    /// `/run/systemd/system` hidden under a tmpfs in a private mount namespace: a system systemd
    /// did not boot.
    Unbooted,
}

const EVERY_WAY: [NoManager; 3] = [NoManager::Offline, NoManager::Chroot, NoManager::Unbooted];

impl NoManager {
    /// What the refusal names as its evidence.
    fn evidence(self) -> &'static str {
        match self {
            NoManager::Offline => "`SYSTEMD_OFFLINE=1` is set",
            NoManager::Chroot => "Running in chroot, ignoring command",
            NoManager::Unbooted => "`/run/systemd/system` does not exist",
        }
    }

    /// `goetia <args>`, elevated as this test is, on a host shown this way.
    fn goetia(self, args: &[&str]) -> Output {
        let goetia = env!("CARGO_BIN_EXE_goetia");
        let mut cmd = match self {
            NoManager::Offline => {
                let mut cmd = Command::new(goetia);
                cmd.env("SYSTEMD_OFFLINE", "1");
                cmd
            }
            NoManager::Chroot => {
                let mut cmd = Command::new(goetia);
                cmd.env("SYSTEMD_IN_CHROOT", "1");
                cmd
            }
            NoManager::Unbooted => {
                let mut cmd = Command::new("unshare");
                cmd.args([
                    "--mount",
                    "--propagation",
                    "private",
                    "/bin/sh",
                    "-c",
                    "mount -t tmpfs none /run/systemd && exec \"$0\" \"$@\"",
                    goetia,
                ]);
                cmd
            }
        };
        cmd.args(args).output().expect("spawn goetia")
    }

    /// Runs `goetia <args>` this way and asserts the refusal: exit `1`, and the one message, naming
    /// this way's evidence. Its stderr.
    fn refuses(self, args: &[&str]) -> String {
        let output = self.goetia(args);
        let (stdout, stderr) = (
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let context = format!("{self:?} {args:?}\nstdout:\n{stdout}\nstderr:\n{stderr}");
        assert_eq!(output.status.code(), Some(1), "{context}");
        assert!(
            stderr.contains("no running systemd manager can be asked here"),
            "{context}"
        );
        assert!(stderr.contains(self.evidence()), "{context}");
        stderr.into_owned()
    }
}

/// RAII removal of the boot-enablement link a test's own `enable` made: declared after the
/// `ServiceGuard`, so it drops first, and the guard's `daemon-reload` sees it gone.
struct RmLink(PathBuf);

impl Drop for RmLink {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// A unit installed on this host's running systemd.
fn installed(guard: &ServiceGuard) -> Id {
    Systemd::new().install(&mk(guard.id()), false).expect("install");
    Id::try_from(guard.id()).unwrap()
}

/// `install` writes nothing, `--start` or not: it is refused before the unit is written.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn an_install_where_no_manager_can_be_asked_writes_nothing() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let (_dir, manifest) = world_readable_manifest(guard.id());
    let manifest = manifest.to_str().expect("utf-8 temp path");

    for way in EVERY_WAY {
        for start in [&[][..], &["--start"], &["--enable"]] {
            let mut args = vec!["daemon", "install", "--file", manifest, guard.id()];
            args.extend_from_slice(start);
            way.refuses(&args);
            assert!(!unit_path(guard.id()).exists(), "{way:?} {args:?} wrote the unit");
            assert!(!wants_symlink(guard.id()).exists(), "{way:?} {args:?} enabled it");
        }
    }
}

/// `start`, `stop` and `restart` send nothing, under every budget: the unit stays where it was,
/// with no job, and a restart leaves the same main process running.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_start_stop_or_restart_where_no_manager_can_be_asked_sends_nothing() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let mgr = Systemd::new();
    let daemon = installed(&guard);
    let budgets: [&[&str]; 3] = [&[], &["--no-timeout"], &["--timeout", "0"]];

    for (verb, before) in [("start", "inactive"), ("stop", "active"), ("restart", "active")] {
        if verb == "stop" {
            mgr.start(&daemon, Budget::DEFAULT).expect("start online");
        }
        for way in EVERY_WAY {
            for budget in budgets {
                let pid = main_pid(guard.id());
                let mut args = vec!["daemon", verb, guard.id()];
                args.extend_from_slice(budget);
                way.refuses(&args);
                assert_eq!(
                    (active_state_and_job(guard.id()), main_pid(guard.id())),
                    ((before.to_string(), String::new()), pid),
                    "{way:?} {args:?} must leave the unit where it was"
                );
            }
        }
    }
}

/// `enable` and `disable` act on unit files where no manager runs, and say nothing: goetia refuses
/// both before either touches the boot-enablement link.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn enable_or_disable_where_no_manager_can_be_asked_leaves_the_link_alone() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let _link = RmLink(wants_symlink(guard.id()));
    let mgr = Systemd::new();
    let daemon = installed(&guard);

    for way in EVERY_WAY {
        way.refuses(&["daemon", "enable", guard.id()]);
        assert!(!wants_symlink(guard.id()).exists(), "{way:?} enable made the link");
    }
    mgr.enable(&daemon).expect("enable online");
    for way in EVERY_WAY {
        way.refuses(&["daemon", "disable", guard.id()]);
        assert!(wants_symlink(guard.id()).exists(), "{way:?} disable removed the link");
    }
    mgr.disable(&daemon).expect("disable online");
}

/// `uninstall` removes nothing and stops nothing: it is refused at its first step, the stop.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn an_uninstall_where_no_manager_can_be_asked_removes_nothing() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let daemon = installed(&guard);
    Systemd::new().start(&daemon, Budget::DEFAULT).expect("start online");

    for way in EVERY_WAY {
        way.refuses(&["daemon", "uninstall", guard.id()]);
        assert!(unit_path(guard.id()).exists(), "{way:?} removed the unit");
        assert_eq!(
            active_state_and_job(guard.id()),
            ("active".to_string(), String::new()),
            "{way:?} stopped it"
        );
    }
}

/// A live-state read is refused rather than read as a state: `status`, by id and for every
/// daemon, and `list`, which reads each daemon's — and so `show`, which renders what `list` finds.
/// An empty answer from a `systemctl` that asked no manager once read as "unknown, not enabled"
/// for a daemon that was running and enabled.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_live_state_read_where_no_manager_can_be_asked_is_refused() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let _link = RmLink(wants_symlink(guard.id()));
    let mgr = Systemd::new();
    let daemon = installed(&guard);
    mgr.enable(&daemon).expect("enable online");
    mgr.start(&daemon, Budget::DEFAULT).expect("start online");

    for way in EVERY_WAY {
        for args in [
            &["daemon", "status", guard.id()][..],
            &["daemon", "status"],
            &["daemon", "list"],
            &["daemon", "show", guard.id()],
        ] {
            way.refuses(args);
        }
        // By id, every other id is still answered: one not installed, from files alone.
        let ghost = format!("{}-ghost", guard.id());
        let stderr = way.refuses(&["daemon", "status", guard.id(), &ghost]);
        assert!(
            stderr.contains(&format!("error: {ghost}: daemon `{ghost}` is not installed")),
            "{way:?}: {stderr}"
        );
        assert!(
            stderr.contains(&format!("error: {}: no running systemd manager", guard.id())),
            "{way:?}: {stderr}"
        );
    }
    mgr.disable(&daemon).expect("disable online");
}

/// The verbs that never reach the manager answer from files alone, as they always did:
/// `install --dry-run`, `diff`, and `show --file`.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn the_verbs_that_never_reach_the_manager_are_untouched() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let (_dir, manifest) = world_readable_manifest(guard.id());
    let manifest = manifest.to_str().expect("utf-8 temp path");
    // From the same manifest, so `diff` has nothing to report.
    let online = Command::new(env!("CARGO_BIN_EXE_goetia"))
        .args(["daemon", "install", "--file", manifest, guard.id()])
        .output()
        .expect("spawn goetia");
    assert!(online.status.success(), "{}", String::from_utf8_lossy(&online.stderr));
    let before = fs::read(unit_path(guard.id())).expect("read the unit");

    for way in EVERY_WAY {
        for args in [
            &["daemon", "install", "--dry-run", "--file", manifest, guard.id()][..],
            &["daemon", "diff", "--file", manifest, guard.id()],
            &["daemon", "show", "--file", manifest, guard.id()],
        ] {
            let output = way.goetia(args);
            assert_eq!(
                output.status.code(),
                Some(0),
                "{way:?} {args:?}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    assert_eq!(fs::read(unit_path(guard.id())).expect("read the unit"), before);
}

/// A real chroot, in `arch-chroot`'s shape: a tmpfs root with `/usr` bound read-only, `/dev`,
/// `/run` and `/sys` bound, a fresh `/proc`, and an `/etc` of its own. `/run` bound in means
/// `/run/systemd/system` exists and the host's manager answers its bus, so `/` not being PID 1's
/// root is the one sign. Every mount lives in a private mount namespace, gone however the script
/// exits. The script copies `manifest` to the chroot's `/tmp/goetia.yaml` and `unit` into its
/// `/etc/systemd/system` (unless `-`), runs `goetia <args>` there, then lists that directory and
/// its `multi-user.target.wants` on stderr.
const IN_A_CHROOT: &str = r#"set -e
root=$1 goetia=$2 manifest=$3 unit=$4; shift 4
mount -t tmpfs goetia-chroot "$root"
mkdir -p "$root/usr" "$root/dev" "$root/run" "$root/sys" "$root/proc" "$root/tmp" "$root/etc/systemd/system"
for dir in bin sbin lib lib64; do ln -s "usr/$dir" "$root/$dir"; done
mount --bind /usr "$root/usr"
mount -o remount,bind,ro "$root/usr"
mount --rbind /dev "$root/dev"
mount --rbind /run "$root/run"
mount --rbind /sys "$root/sys"
mount -t proc proc "$root/proc"
cp /etc/passwd /etc/group /etc/nsswitch.conf "$root/etc/"
cp "$goetia" "$root/goetia"
cp "$manifest" "$root/tmp/goetia.yaml"
[ "$unit" = - ] || cp "$unit" "$root/etc/systemd/system/"
rc=0
chroot "$root" /goetia "$@" || rc=$?
echo "chroot units: $(cd "$root/etc/systemd/system" && ls -A | tr '\n' ' ')" >&2
echo "chroot links: $(ls -A "$root/etc/systemd/system/multi-user.target.wants" 2>/dev/null | tr '\n' ' ')" >&2
exit $rc
"#;

/// Every verb that reaches the manager, run in a real chroot, is refused up front — naming the
/// chroot goetia found itself, before any `systemctl` runs, so never through `systemctl`'s own
/// "Running in chroot" — and writes nothing into the chroot, and asks the host's manager nothing.
/// The daemon's unit is installed on the host, and copied into the chroot for every verb but
/// `install`.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn every_verb_in_a_real_chroot_is_refused_before_anything_runs() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let (_manifest_dir, manifest) = world_readable_manifest(guard.id());
    installed(&guard);
    let unit = unit_path(guard.id());
    let unit = unit.to_str().expect("utf-8 unit path");
    let copied = format!("chroot units: {}.service \n", guard.id());

    let installs = ["daemon", "install", "--file", "/tmp/goetia.yaml", guard.id()];
    for (args, unit, units) in [
        (&installs[..], "-", "chroot units: \n"),
        (
            &[
                "daemon",
                "install",
                "--file",
                "/tmp/goetia.yaml",
                "--enable",
                "--start",
                guard.id(),
            ],
            "-",
            "chroot units: \n",
        ),
        (&["daemon", "uninstall", guard.id()], unit, &copied),
        (&["daemon", "start", guard.id()], unit, &copied),
        (&["daemon", "stop", guard.id()], unit, &copied),
        (&["daemon", "restart", guard.id()], unit, &copied),
        (&["daemon", "restart", guard.id(), "--timeout", "0"], unit, &copied),
        (&["daemon", "enable", guard.id()], unit, &copied),
        (&["daemon", "disable", guard.id()], unit, &copied),
        (&["daemon", "status", guard.id()], unit, &copied),
        (&["daemon", "list"], unit, &copied),
    ] {
        let root = tempfile::tempdir().expect("tempdir");
        let output = Command::new("unshare")
            .args([
                "--mount",
                "--propagation",
                "private",
                "/bin/sh",
                "-c",
                IN_A_CHROOT,
                "goetia-chroot",
            ])
            .arg(root.path())
            .arg(env!("CARGO_BIN_EXE_goetia"))
            .arg(&manifest)
            .arg(unit)
            .args(args)
            .output()
            .expect("spawn unshare");
        let stderr = String::from_utf8_lossy(&output.stderr);
        let context = format!("{args:?}\nstderr:\n{stderr}");
        assert_eq!(output.status.code(), Some(1), "{context}");
        assert!(
            stderr.contains("no running systemd manager can be asked here (`/` is not PID 1's root"),
            "{context}"
        );
        assert!(!stderr.contains("Running in chroot"), "systemctl ran: {context}");
        assert!(stderr.contains(units), "{context}");
        assert!(stderr.contains("chroot links: \n"), "{context}");
        assert_eq!(
            active_state_and_job(guard.id()),
            ("inactive".to_string(), String::new()),
            "{context}"
        );
        assert!(!wants_symlink(guard.id()).exists(), "{context}");
    }
}

//! Where no systemd manager can be asked — systemd's offline mode, a chroot, a system systemd did
//! not boot — every verb that reaches the manager refuses, exit `1`, before it writes or sends
//! anything, with one message naming what showed it. The verbs that never reach the manager are
//! untouched.

use std::fs;
use std::path::{Path, PathBuf};
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
    /// A `systemctl` that reports a chroot in these words, and only its report says so: a stand-in
    /// first on `PATH` ([`stand_in`]). `SYSTEMD_OFFLINE=1` would make a real one report on every
    /// supported systemd, but it cannot serve here: goetia reads it itself and refuses with
    /// [`NoManager::Offline`]'s evidence before it spawns anything, which is the case above.
    Reported(&'static str),
    /// `/run/systemd/system` hidden under a tmpfs in a private mount namespace: a system systemd
    /// did not boot.
    Unbooted,
}

/// What `systemctl` in a chroot says for a verb, `$1`, it ignored: 246 and newer, 242 to 245, and
/// every version with no verb to name (systemd's `verbs.c` and `systemctl.c`).
const REPORTS: [&str; 3] = [
    "Running in chroot, ignoring command '$1'",
    "Running in chroot, ignoring request: $1",
    "Running in chroot, ignoring request.",
];

const EVERY_WAY: [NoManager; 5] = [
    NoManager::Offline,
    NoManager::Reported(REPORTS[0]),
    NoManager::Reported(REPORTS[1]),
    NoManager::Reported(REPORTS[2]),
    NoManager::Unbooted,
];

/// How the refusal starts, before its evidence.
const REFUSAL: &str = "goetia does not manage systemd here (";

impl NoManager {
    /// What the refusal names as its evidence. Every verb that reaches the manager asks it with
    /// `show` first — a state read, or the version gate before a request — so that is the verb a
    /// stand-in reports.
    fn evidence(self) -> String {
        match self {
            NoManager::Offline => "`SYSTEMD_OFFLINE=1` is set".to_string(),
            NoManager::Reported(report) => format!("`systemctl` said \"{}\"", report.replace("$1", "show")),
            NoManager::Unbooted => {
                "`/run/systemd/system`, which systemd makes when it boots a system, does not exist here".to_string()
            }
        }
    }

    /// `goetia <args>`, elevated as this test is, on a host shown this way.
    fn goetia(self, args: &[&str]) -> Output {
        let goetia = env!("CARGO_BIN_EXE_goetia");
        // Kept until goetia has exited: the stand-in's directory.
        let mut _stand_in = None;
        let mut cmd = match self {
            NoManager::Offline => {
                let mut cmd = Command::new(goetia);
                cmd.env("SYSTEMD_OFFLINE", "1");
                cmd
            }
            NoManager::Reported(report) => {
                let dir = stand_in(report);
                let mut cmd = Command::new(goetia);
                cmd.env("PATH", path_first(dir.path()));
                // Inherited by goetia, and by every `systemctl` it does not clear them from: three
                // of them turn a real chroot detection off, one names another root's bus, and the
                // rest no systemd reads at all — two of them unlistable — so what the stand-in
                // models is goetia's removal by prefix, not systemd's reading.
                for (switch, value) in every_silencer() {
                    cmd.env(switch, value);
                }
                _stand_in = Some(dir);
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
        assert!(stderr.contains(&format!("{REFUSAL}{}", self.evidence())), "{context}");
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

/// The names goetia must take off the `systemctl` it spawns here, with a value that would matter.
/// The first three turn a real `systemctl`'s own chroot detection off:
/// `running_in_chroot_or_offline()` consults `SYSTEMD_OFFLINE` first (every supported version, and
/// a *false* value short-circuits the chroot check as surely as a true one short-circuits
/// everything), then `SYSTEMD_IN_CHROOT` (257 on), then `SYSTEMD_IGNORE_CHROOT`. The fourth is a
/// switch no systemd has, so the removal is shown to reach past the switches systemd ships today.
/// The fifth is from systemd's other switch namespace for this binary, the one that sends a real
/// `systemctl` to the bus `DBUS_SYSTEM_BUS_ADDRESS` names rather than this root's manager. Setting
/// them all on goetia proves the removal rather than assuming it — without it, the stand-in below
/// looks away and there is no report for goetia to refuse on.
///
/// What no name written here can show is that the removal goes by *prefix*: a denylist holding
/// exactly these five literals would satisfy every assertion that rests on them alone.
/// [`unlistable`] is what closes that, and is set on goetia alongside these — so such a denylist
/// does NOT pass this file as it stands, and is measured failing six of its tests.
const SILENCERS: [(&str, &str); 5] = [
    ("SYSTEMD_IGNORE_CHROOT", "1"),
    ("SYSTEMD_IN_CHROOT", "0"),
    ("SYSTEMD_OFFLINE", "0"),
    ("SYSTEMD_A_SWITCH_NO_SUPPORTED_VERSION_HAS_YET", "1"),
    ("SYSTEMCTL_FORCE_BUS", "1"),
];

/// One more [`SILENCERS`] name under each prefix goetia removes, made at run time from this test
/// process's pid: no denylist of literal names — the shape goetia shipped and had to correct twice
/// — can contain them, so a child that arrives without them arrives without them for their prefix.
/// Set on goetia and honoured by the stand-in exactly as [`SILENCERS`] are.
fn unlistable() -> [String; 2] {
    let pid = std::process::id();
    [format!("SYSTEMD_H{pid}"), format!("SYSTEMCTL_H{pid}")]
}

/// [`SILENCERS`] and [`unlistable`] together: every name goetia must take off the child here.
fn every_silencer() -> Vec<(String, String)> {
    SILENCERS
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .chain(unlistable().map(|name| (name, "1".to_string())))
        .collect()
}

/// `dir` ahead of the inherited `PATH`.
fn path_first(dir: &Path) -> std::ffi::OsString {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut path = std::env::split_paths(&path).collect::<Vec<_>>();
    path.insert(0, dir.to_path_buf());
    std::env::join_paths(path).expect("PATH")
}

/// What the stand-in checks before it reports a chroot, as shell: that it carries none of
/// [`SILENCERS`] nor [`unlistable`], by exiting `0` silently if any survived — as a real `systemctl`
/// that believed it was not in a chroot and went on to do the work would. Stricter than a real one,
/// which honours only the names it knows, and deliberately so: what is pinned is goetia's removal,
/// not systemd's reading, so the three names no systemd would act on here must silence it too. The
/// three it does read are matched over every word `parse_boolean` accepts —
/// `1|yes|y|true|t|on` and `0|no|n|false|f|off` — rather than only the value [`SILENCERS`] sets, so
/// a child given one of the other spellings is caught as well. Lower case only, where
/// `parse_boolean` compares case-insensitively: `SILENCERS` sets `1` and `0` and nothing else, so no
/// child is ever given `YES` or `Off`, and a stand-in that looks away less readily than a real
/// `systemctl` can only make these tests stricter.
fn honours_the_silencers() -> String {
    let mut shell = String::from(
        "case \"${SYSTEMD_OFFLINE-}\" in 0|no|n|false|f|off) exit 0;; esac\n\
         case \"${SYSTEMD_IN_CHROOT-}\" in 0|no|n|false|f|off) exit 0;; esac\n\
         case \"${SYSTEMD_IGNORE_CHROOT-}\" in 1|yes|y|true|t|on) exit 0;; esac\n\
         [ -n \"${SYSTEMD_A_SWITCH_NO_SUPPORTED_VERSION_HAS_YET-}\" ] && exit 0\n\
         [ -n \"${SYSTEMCTL_FORCE_BUS-}\" ] && exit 0\n",
    );
    for name in unlistable() {
        shell.push_str(&format!("[ -n \"${{{name}-}}\" ] && exit 0\n"));
    }
    shell
}

/// A stand-in `systemctl` that says `report` on stderr, `$1` the verb, for every verb but
/// `--version`, and exits `0`, as a real one in a chroot does; `--version` it answers, as a real
/// one does there. It runs nothing else, so it never reaches the manager.
fn stand_in(report: &str) -> tempfile::TempDir {
    let honours = honours_the_silencers();
    written(&format!(
        "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo 'systemd 255 (255)'; exit 0; }}\n\
         {honours}echo \"{report}\" >&2\n"
    ))
}

/// A stand-in `systemctl` that answers the version gate — `--version` for the client, the `Version`
/// property for the manager — and reports `report` for anything else, honouring [`SILENCERS`] and
/// [`unlistable`]. So `door` and the gate both pass, and only the request itself is ignored.
fn gate_then_stand_in(report: &str) -> tempfile::TempDir {
    let honours = honours_the_silencers();
    written(&format!(
        "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo 'systemd 255 (255)'; exit 0; }}\n\
         [ \"$1\" = show ] && {{ echo 255; exit 0; }}\n\
         {honours}echo \"{report}\" >&2\n"
    ))
}

/// A directory holding `script` as its `systemctl`. Written by a child process, so no descriptor
/// open for writing on it can be inherited by a `fork` another test thread makes, and fail its
/// `exec` with `ETXTBSY`.
fn written(script: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let wrote = Command::new("/bin/sh")
        .args([
            "-c",
            "printf '%s' \"$0\" > \"$1/systemctl\" && chmod 755 \"$1/systemctl\"",
            script,
        ])
        .arg(dir.path())
        .status()
        .expect("spawn sh");
    assert!(wrote.success(), "writing the stand-in failed: {wrote:?}");
    dir
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
            stderr.contains(&format!("error: {}: {REFUSAL}", guard.id())),
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

/// A real chroot, in `arch-chroot`'s shape: a root with `/usr` bound read-only, `/dev`, `/run` and
/// `/sys` bound, a fresh `/proc`, and an `/etc` of its own. `/run` bound in means
/// `/run/systemd/system` exists and the host's manager answers its bus, so `/` not being PID 1's
/// root is the one sign. Every mount lives in a private mount namespace, gone however the script
/// exits. The script copies `manifest` to the chroot's `/tmp/goetia.yaml` and `unit` into its
/// `/etc/systemd/system` (unless `-`), runs `goetia <args>` there — as `user`, a `chroot
/// --userspec`, unless `-` — then lists that directory and its `multi-user.target.wants` on
/// stderr. The root is a tmpfs for `shape` `tmpfs`, and for `dir` the directory itself, no mount;
/// for `bare` a tmpfs with no `/run` bound in, as a `debootstrap` chroot has none; and for
/// `dir-hidepid` a directory whose `/proc` is mounted `hidepid=2`, hiding PID 1 from other users.
const IN_A_CHROOT: &str = r#"set -e
root=$1 goetia=$2 manifest=$3 unit=$4 user=$5 shape=$6; shift 6
case $shape in dir*) ;; *) mount -t tmpfs goetia-chroot "$root" ;; esac
chmod 755 "$root"
mkdir -p "$root/usr" "$root/dev" "$root/run" "$root/sys" "$root/proc" "$root/tmp" "$root/etc/systemd/system"
for dir in bin sbin lib lib64; do ln -s "usr/$dir" "$root/$dir"; done
mount --bind /usr "$root/usr"
mount -o remount,bind,ro "$root/usr"
mount --rbind /dev "$root/dev"
[ "$shape" = bare ] || mount --rbind /run "$root/run"
mount --rbind /sys "$root/sys"
case $shape in dir-hidepid) mount -t proc -o hidepid=2 proc "$root/proc" ;; *) mount -t proc proc "$root/proc" ;; esac
cp /etc/passwd /etc/group /etc/nsswitch.conf "$root/etc/"
cp "$goetia" "$root/goetia"
cp "$manifest" "$root/tmp/goetia.yaml"
[ "$unit" = - ] || cp "$unit" "$root/etc/systemd/system/"
rc=0
if [ "$user" = - ]; then
    chroot "$root" /goetia "$@" || rc=$?
else
    chroot --userspec="$user" "$root" /goetia "$@" || rc=$?
fi
echo "chroot units: $(cd "$root/etc/systemd/system" && ls -A | tr '\n' ' ')" >&2
echo "chroot links: $(ls -A "$root/etc/systemd/system/multi-user.target.wants" 2>/dev/null | tr '\n' ' ')" >&2
exit $rc
"#;

/// [`IN_A_CHROOT`], in a private mount namespace, on a root `root`: `goetia <args>` in a chroot of
/// `shape`, run as `user`, with `manifest` and `unit` copied in.
fn in_a_chroot(root: &Path, manifest: &Path, unit: &str, user: &str, shape: &str, args: &[&str]) -> Output {
    Command::new("unshare")
        .args([
            "--mount",
            "--propagation",
            "private",
            "/bin/sh",
            "-c",
            IN_A_CHROOT,
            "goetia-chroot",
        ])
        .arg(root)
        .arg(env!("CARGO_BIN_EXE_goetia"))
        .arg(manifest)
        .args([unit, user, shape])
        .args(args)
        .output()
        .expect("spawn unshare")
}

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
        let output = in_a_chroot(root.path(), &manifest, unit, "-", "tmpfs", args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let context = format!("{args:?}\nstderr:\n{stderr}");
        assert_eq!(output.status.code(), Some(1), "{context}");
        assert!(
            stderr.contains(&format!("{REFUSAL}`/` is not PID 1's root")),
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

/// Unelevated, in a real chroot with `/run` bound in, `status` and `list` — the verbs that need no
/// elevation — are refused up front too, before any `systemctl` runs. There goetia may not read
/// `/proc/1/root`, and neither may `systemctl`, which then assumes no chroot and asks the host's
/// manager over the bound `/run` about a unit the host does not have: once read as "stopped, not
/// enabled", exit `0`. The mount tables are what show it, `/` in goetia's naming none of PID 1's:
/// a tmpfs root's mount is another, and a directory that is no mount has none at `/` — which
/// goetia's own table shows even where a `hidepid=2` `/proc` hides PID 1's. The daemon's unit is
/// goetia's, in the chroot only.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn an_unelevated_state_read_in_a_real_chroot_is_refused_before_anything_runs() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let (_manifest_dir, manifest) = world_readable_manifest(guard.id());
    let daemon = installed(&guard);
    let unit_dir = tempfile::tempdir().expect("tempdir");
    let unit = unit_dir.path().join(format!("{}.service", guard.id()));
    fs::copy(unit_path(guard.id()), &unit).expect("copy the unit");
    Systemd::new().uninstall(&daemon).expect("uninstall from the host");
    let unit = unit.to_str().expect("utf-8 temp path");

    for (shape, evidence) in [
        ("tmpfs", "the mount at `/` is none of PID 1's"),
        ("dir", "no mount is at `/`"),
        ("dir-hidepid", "no mount is at `/`"),
    ] {
        for args in [
            &["daemon", "status", guard.id()][..],
            &["daemon", "status"],
            &["daemon", "list"],
        ] {
            let root = tempfile::tempdir().expect("tempdir");
            let output = in_a_chroot(root.path(), &manifest, unit, "65534:65534", shape, args);
            let (stdout, stderr) = (
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            let context = format!("{shape} {args:?}\nstdout:\n{stdout}\nstderr:\n{stderr}");
            assert_eq!(output.status.code(), Some(1), "{context}");
            assert!(stderr.contains(&format!("{REFUSAL}{evidence}")), "{context}");
            assert!(stdout.is_empty(), "{context}");
            assert!(!stderr.contains("Running in chroot"), "systemctl ran: {context}");
        }
    }
    assert!(!unit_path(guard.id()).exists(), "the host has no such unit");
}

/// A chroot with no `/run` bound in, as a `debootstrap` one has none, is refused as the chroot it
/// is: `/run/systemd/system` is missing there too, but systemd did boot this machine, and the
/// chroot is the more specific cause. Elevated `/proc/1/root` shows it, and unelevated the mount
/// tables do.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_chroot_without_run_is_refused_as_a_chroot() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let (_manifest_dir, manifest) = world_readable_manifest(guard.id());
    installed(&guard);
    let unit = unit_path(guard.id());
    let unit = unit.to_str().expect("utf-8 unit path");

    for (user, args, evidence) in [
        ("-", &["daemon", "status", guard.id()][..], "`/` is not PID 1's root"),
        ("-", &["daemon", "start", guard.id()], "`/` is not PID 1's root"),
        (
            "65534:65534",
            &["daemon", "status", guard.id()],
            "the mount at `/` is none of PID 1's",
        ),
    ] {
        let root = tempfile::tempdir().expect("tempdir");
        let output = in_a_chroot(root.path(), &manifest, unit, user, "bare", args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let context = format!("{user} {args:?}\nstderr:\n{stderr}");
        assert_eq!(output.status.code(), Some(1), "{context}");
        assert!(stderr.contains(&format!("{REFUSAL}{evidence}")), "{context}");
        assert!(!stderr.contains("/run/systemd/system"), "{context}");
    }
    assert_eq!(
        active_state_and_job(guard.id()),
        ("inactive".to_string(), String::new())
    );
}

/// The other place goetia spawns a `systemctl`: the watched one a bounded `start` runs, built
/// through cosca rather than `Command`, so its environment is set in its own place. The stand-in
/// answers the version gate, so nothing but that spawn's own capture can produce a refusal, and it
/// honours [`SILENCERS`] and [`unlistable`] — all of which goetia carries in its own environment
/// here. A child that kept them looks away, exits `0` having announced no job, and goetia reports a
/// start that never happened.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_watched_request_a_systemctl_ignored_is_refused_whatever_the_environment_said() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let _daemon = installed(&guard);
    let before = (active_state_and_job(guard.id()), main_pid(guard.id()));
    let report = "Running in chroot, ignoring command 'start'";
    let dir = gate_then_stand_in(report);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_goetia"));
    cmd.env("PATH", path_first(dir.path()));
    for (switch, value) in every_silencer() {
        cmd.env(switch, value);
    }
    // No budget flag: `Budget::DEFAULT` waits and has a deadline, which is what `watched` is.
    let output = cmd
        .args(["daemon", "start", guard.id()])
        .output()
        .expect("spawn goetia");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let context = format!(
        "stdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(output.status.code(), Some(1), "{context}");
    assert!(
        stderr.contains(&format!("{REFUSAL}`systemctl` said \"{report}\"")),
        "{context}"
    );
    assert_eq!(
        (active_state_and_job(guard.id()), main_pid(guard.id())),
        before,
        "{context}"
    );
}

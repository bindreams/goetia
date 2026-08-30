//! launchd: does it accept our plist, and is `Disabled` honoured as a default?
//!
//! The metadata site is an XML comment after the DOCTYPE — inert by
//! construction, since the parser strips comments before launchd sees a dict.
//! "By construction" is still a claim about Apple's parser, so it is measured
//! here, together with the documented-nowhere fallback (unknown top-level
//! keys) and the `Disabled` default that install-off-by-default rests on.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use crate::support::{self, ConnectBack, ELEVATED, ServiceGuard, cmd};

// Probes ==============================================================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn launchd_xml_comment_is_inert() {
    let label = support::random_test_id();
    let guard = ServiceGuard::new(&label);
    let blob = support::probe_blob();
    let text = plist(guard.id(), &comment_block(&blob), "", &["/bin/sh", "-c", "exit 0"]);

    write_plist(guard.id(), &text);
    let boot = bootstrap(guard.id());
    assert!(
        boot.ok(),
        "launchd rejected a plist carrying the metadata comment:\n{boot}"
    );

    // The anchor that keeps this from passing on a `launchctl` that merely
    // exits 0: the job has to actually be in the system domain afterwards.
    // `launchd_malformed_plist_is_rejected` proves this check discriminates.
    let print = print(guard.id());
    assert!(
        print.ok(),
        "bootstrap reported success but the job is not registered:\n{print}"
    );

    let on_disk = fs::read_to_string(plist_path(guard.id())).expect("read back plist");
    assert_eq!(on_disk, text, "launchd rewrote the plist");

    bootout(guard.id()).expect_ok();

    support::record_probe(
        "launchd-xml-comment",
        &format!(
            "site: XML comment after the DOCTYPE\ninert: yes\nblob_len: {} base64 chars\nos: {}\n",
            blob.len(),
            sw_vers(),
        ),
    );
}

/// The control for `launchd_xml_comment_is_inert`: a plist launchd must refuse.
/// Without it, "bootstrap succeeded and the job is present" carries no
/// information, because nothing shows the checks can come out the other way.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn launchd_malformed_plist_is_rejected() {
    let label = support::random_test_id();
    let guard = ServiceGuard::new(&label);
    let text = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\t<key>Label</key><string>{label}</string>\n"
    );

    write_plist(guard.id(), &text);
    let boot = bootstrap(guard.id());
    let print = print(guard.id());
    assert!(
        !boot.ok() || !print.ok(),
        "launchd accepted a truncated plist and registered the job; the inertness probe's \
         success checks cannot distinguish acceptance from rejection:\n{boot}\n{print}"
    );
}

/// Measuring, not asserting: the fallback site is only needed if the comment
/// site fails, so an outcome either way is data rather than a build failure.
/// The result is written to `probe-results/`, which CI uploads — otherwise
/// "not asserted" quietly becomes "never checked again".
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn launchd_unknown_top_level_keys_are_tolerated() {
    let label = support::random_test_id();
    let guard = ServiceGuard::new(&label);
    let blob = support::probe_blob();
    let extra = format!(
        "\t<key>com.goetia.Marker</key>\n\t<string>goetia</string>\n\
         \t<key>com.goetia.Schema</key>\n\t<integer>1</integer>\n\
         \t<key>com.goetia.Spec</key>\n\t<string>{blob}</string>\n"
    );
    let text = plist(guard.id(), "", &extra, &["/bin/sh", "-c", "exit 0"]);

    write_plist(guard.id(), &text);
    let boot = bootstrap(guard.id());
    let print = print(guard.id());
    let tolerated = boot.ok() && print.ok();
    if tolerated {
        bootout(guard.id()).expect_ok();
    }

    let on_disk = fs::read_to_string(plist_path(guard.id())).expect("read back plist");
    assert_eq!(on_disk, text, "launchd rewrote the plist");

    support::record_probe(
        "launchd-unknown-top-level-keys",
        &format!(
            "site: `com.goetia.*` top-level plist keys (fallback)\n\
             tolerated: {tolerated}\n\
             bootstrap_exit: {:?}\n\
             bootstrap_stderr: {}\n\
             print_exit: {:?}\n\
             os: {}\n",
            boot.code,
            boot.stderr.trim(),
            print.code,
            sw_vers(),
        ),
    );
}

/// `install` writes the plist and does not enable at boot; on launchd that is
/// seeded by `Disabled: true` being a *default* that the override database
/// supersedes. If launchd ignores the key, or if `launchctl enable` does not
/// survive a plist rewrite, macOS needs a different mechanism entirely.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn launchd_disabled_key_default_applies() {
    let label = support::random_test_id();
    let guard = ServiceGuard::new(&label);

    // Control: the same plist minus the one key. It proves the plist, the
    // program, and this runner's ability to bootstrap and run a system daemon
    // all work — so a job that does not run in phase 1 can only be `Disabled`
    // doing it, and phase 1 cannot pass because bootstrap happened to fail for
    // some unrelated reason.
    let control = ConnectBack::listen();
    write_plist(guard.id(), &probe_job(guard.id(), control.port(), false));
    bootstrap(guard.id()).expect_ok();
    control.accept("the control job, which carries no `Disabled` key, to start");
    bootout(guard.id()).expect_ok();

    // Phase 1: `Disabled: true`, no override entry. The job must not run.
    let first = ConnectBack::listen();
    write_plist(guard.id(), &probe_job(guard.id(), first.port(), true));
    let boot_disabled = bootstrap(guard.id());
    let state_before = disabled_state(guard.id());
    if boot_disabled.ok() {
        // `bootout` removes the job from the domain and does not return until
        // it has. Past that point the job can never run, so the check below is
        // a proof of absence rather than a bet on how long to wait.
        bootout(guard.id()).expect_ok();
    }
    assert!(
        !first.connected_yet(),
        "launchd started a job whose plist says `Disabled: true` and which has no override-DB \
         entry; install-off-by-default cannot be seeded from this key"
    );

    // Phase 2: an override entry supersedes the plist default.
    cmd::run("launchctl", &["enable", &target(guard.id())]).expect_ok();
    bootstrap(guard.id()).expect_ok();
    first.accept("the launchd job to start after `launchctl enable`");
    let state_after = disabled_state(guard.id());
    assert_eq!(
        state_after,
        Some(false),
        "`launchctl enable` did not clear the disabled state launchd reports for the label"
    );

    // Phase 3: rewriting the plist must not discard the override — install is
    // a routine operation and must never silently re-disable a service.
    let second = ConnectBack::listen();
    bootout(guard.id()).expect_ok();
    write_plist(guard.id(), &probe_job(guard.id(), second.port(), true));
    bootstrap(guard.id()).expect_ok();
    second.accept("the launchd job to start after the plist was rewritten");

    support::record_probe(
        "launchd-disabled-default",
        &format!(
            "control_job_without_the_key_ran: yes\n\
             disabled_plist_key_honoured_as_default: yes\n\
             bootstrap_of_disabled_job_exit: {:?}\n\
             bootstrap_of_disabled_job_stderr: {}\n\
             print_disabled_before_enable: {state_before:?}\n\
             print_disabled_after_enable: {state_after:?}\n\
             override_survives_plist_rewrite: yes\n\
             os: {}\n",
            boot_disabled.code,
            boot_disabled.stderr.trim(),
            sw_vers(),
        ),
    );
}

// Helpers -------------------------------------------------------------------------------------------------------------

fn plist_path(label: &str) -> PathBuf {
    PathBuf::from(support::LAUNCHD_DAEMON_DIR).join(format!("{label}.plist"))
}

/// launchd refuses plists that are group- or world-writable, so the mode is
/// set explicitly rather than left to whatever umask the runner has.
fn write_plist(label: &str, text: &str) {
    let path = plist_path(label);
    fs::write(&path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod plist");
}

fn plist(label: &str, comment: &str, extra_keys: &str, program: &[&str]) -> String {
    let args: String = program
        .iter()
        .map(|arg| format!("\t\t<string>{arg}</string>\n"))
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         {comment}\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\t<string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n{args}\t</array>\n\
         {extra_keys}\
         </dict>\n\
         </plist>\n"
    )
}

fn comment_block(blob: &str) -> String {
    format!("<!-- goetia:begin\nMarker: goetia\nSchema: 1\nSpec: {blob}\ngoetia:end -->\n")
}

/// `RunAtLoad: true`, so the daemon reports in over loopback the moment
/// launchd is willing to run it. There is no way to observe "it did not run"
/// without something that would unmistakably have been observed if it had.
/// `disabled` is the only thing that varies between the control and phase 1.
fn probe_job(label: &str, port: u16, disabled: bool) -> String {
    let key = if disabled {
        "\t<key>Disabled</key>\n\t<true/>\n"
    } else {
        ""
    };
    let extra = format!("{key}\t<key>RunAtLoad</key>\n\t<true/>\n");
    let exe = support::current_exe_str();
    let port = port.to_string();
    plist(label, "", &extra, &[&exe, support::sentinel::CONNECT_BACK, &port])
}

fn target(label: &str) -> String {
    format!("system/{label}")
}

fn bootstrap(label: &str) -> cmd::Run {
    cmd::run(
        "launchctl",
        &["bootstrap", "system", plist_path(label).to_str().unwrap()],
    )
}

fn bootout(label: &str) -> cmd::Run {
    cmd::run("launchctl", &["bootout", &target(label)])
}

fn print(label: &str) -> cmd::Run {
    cmd::run("launchctl", &["print", &target(label)])
}

/// launchd's own view of whether the label is disabled, which is the union of
/// the plist default and the override database. `None` means launchd does not
/// know the label at all.
fn disabled_state(label: &str) -> Option<bool> {
    let run = cmd::run("launchctl", &["print-disabled", "system"]);
    let line = run.stdout.lines().find(|line| line.contains(label))?;
    if line.contains("true") || line.contains("disabled") {
        Some(true)
    } else if line.contains("false") || line.contains("enabled") {
        Some(false)
    } else {
        None
    }
}

fn sw_vers() -> String {
    cmd::run("sw_vers", &["-productVersion"]).stdout.trim().to_string()
}

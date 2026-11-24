// The order in which Task Scheduler's Stop reaches a console action and a program it started (goetia's shim and its
// program), `tsprobe order ...`. Driven by `order.ps1`.
//
//   order parent <log> <create> <child-beh> <parent-beh> <shape>
//       the action: installs a console handler that logs and never returns, starts one child on the same console,
//       logs when it saw the child's exit, then either ends its own process (parent-beh p1, code 42) or keeps running
//       (p2).
//         create    how the child is created: none (no flags), group (CREATE_NEW_PROCESS_GROUP), cosca (cosca 0.10,
//                   `contain_with(BestEffort)`, stdout and stderr null: how goetia's shim spawns a task's program)
//         child-beh what the child does with CTRL_CLOSE_EVENT: a ends its process at once (code 5), b ends it 300 ms
//                   after its handler was entered (code 6), c never returns from the handler and never exits, d has no
//                   handler, e returns TRUE at once
//         shape     single, or launcher: the child starts a grandchild (std, no flags, same console) with the same
//                   behaviour
//   order child <log> <role> <beh> <leaf|launcher>
//
// Every line: `role=<r> pid=<p> t=<GetSystemTimePreciseAsFileTime> <what>`; one clock for every process. A wait here is on
// a handle or an event; the 300 ms of behaviour b is the experiment's independent variable, not a synchronisation.
use crate::util::*;
use std::sync::OnceLock;
use windows::Win32::Foundation::*;
use windows::Win32::System::Console::*;
use windows::Win32::System::Threading::*;

static LOG: OnceLock<String> = OnceLock::new();
static ROLE: OnceLock<String> = OnceLock::new();
static BEH: OnceLock<String> = OnceLock::new();
static NEVER: OnceLock<isize> = OnceLock::new();

fn say(what: &str) {
    use std::io::Write;
    // One write per line, so that the processes sharing the log cannot split each other's lines (`append` writes the
    // newline separately).
    let line = format!("role={} pid={} t={} {what}\n", ROLE.get().unwrap(), std::process::id(), now_ft());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(LOG.get().unwrap()) {
        let _ = f.write_all(line.as_bytes());
    }
}

fn never_event() -> HANDLE {
    HANDLE(*NEVER.get_or_init(|| {
        // SAFETY: an unnamed manual-reset event that nothing sets.
        unsafe { CreateEventW(None, true, false, windows::core::PCWSTR::null()).expect("CreateEventW").0 as isize }
    }) as *mut core::ffi::c_void)
}

fn forever() -> ! {
    // SAFETY: a wait on an event owned here and never set.
    unsafe { WaitForSingleObject(never_event(), INFINITE) };
    unreachable!("nothing sets the event")
}

unsafe extern "system" fn handler(event: u32) -> windows::core::BOOL {
    say(&format!("handler-enter event={event}"));
    if event != 2 {
        return false.into();
    }
    if ROLE.get().unwrap() == "parent" {
        forever();
    }
    match BEH.get().unwrap().as_str() {
        "a" => {
            say("about-to-exit code=5");
            ExitProcess(5)
        }
        "b" => {
            // The experiment's delay: the process ends 300 ms after its handler was entered.
            Sleep(300);
            say("about-to-exit code=6");
            ExitProcess(6)
        }
        "c" => forever(),
        _ => true.into(),
    }
}

pub fn run(a: &[String]) -> i32 {
    match a.first().map(String::as_str) {
        Some("parent") => parent(&a[1..]),
        Some("child") => child(&a[1..]),
        _ => 2,
    }
}

fn install(log: &str, role: &str, beh: &str) {
    LOG.set(log.to_owned()).ok();
    ROLE.set(role.to_owned()).ok();
    BEH.set(beh.to_owned()).ok();
}

fn child(a: &[String]) -> i32 {
    let (log, role, beh, shape) = (&a[0], &a[1], &a[2], &a[3]);
    install(log, role, beh);
    if beh != "d" {
        // SAFETY: the handler is a plain function that outlives the process.
        unsafe {
            let _ = SetConsoleCtrlHandler(Some(handler), true);
        }
    }
    say(&format!("start beh={beh}"));
    if shape == "launcher" {
        let exe = std::env::current_exe().unwrap();
        let grand = std::process::Command::new(exe)
            .args(["order", "child", log, "grandchild", beh, "leaf"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match grand {
            Ok(g) => {
                say(&format!("grandchild-spawned pid={}", g.id()));
                std::mem::forget(g);
            }
            Err(e) => say(&format!("grandchild-spawn-failed {e}")),
        }
    }
    say("ready");
    forever()
}

fn parent(a: &[String]) -> i32 {
    let (log, create, child_beh, parent_beh, shape) = (&a[0], &a[1], &a[2], &a[3], &a[4]);
    install(log, "parent", parent_beh);
    // SAFETY: the handler is a plain function that outlives the process.
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(handler), true);
    }
    say(&format!("start create={create} child={child_beh} parent={parent_beh} shape={shape}"));
    let exe = std::env::current_exe().unwrap();
    let args: Vec<String> = ["order", "child", log, "child", child_beh, if shape == "launcher" { "launcher" } else { "leaf" }]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let code = match create.as_str() {
        "cosca" => {
            let mut cmd = cosca::run(std::iter::once(exe.display().to_string()).chain(args));
            cmd.contain_with(cosca::ContainMode::BestEffort);
            cmd.stdout(cosca::Stdio::null()).expect("stdout");
            cmd.stderr(cosca::Stdio::null()).expect("stderr");
            let child = cmd.spawn().expect("cosca spawn");
            say(&format!("child-spawned pid={} containment={:?}", child.pid(), child.containment()));
            say("ready");
            let status = child.wait();
            let code = status.as_ref().ok().and_then(|s| s.code());
            // Kept alive to the end of the process, as the shim keeps its child.
            std::mem::forget(child);
            code
        }
        other => {
            use std::os::windows::process::CommandExt;
            let mut cmd = std::process::Command::new(exe);
            cmd.args(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
            if other == "group" {
                cmd.creation_flags(0x0000_0200);
            }
            let mut child = cmd.spawn().expect("std spawn");
            say(&format!("child-spawned pid={}", child.id()));
            say("ready");
            child.wait().ok().and_then(|s| s.code())
        }
    };
    say(&format!("saw-child-exit code={}", code.map_or("none".to_owned(), |c| c.to_string())));
    if parent_beh == "p1" {
        say("about-to-exit code=42");
        // SAFETY: plain call.
        unsafe { ExitProcess(42) }
    }
    forever()
}

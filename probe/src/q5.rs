use crate::q3::*;
use crate::util::*;
use std::time::{Duration, Instant};

const LABEL: &str = "com.goetia.probe.q5";

pub fn run_q5() {
    hdr("Q5: what else is there?");

    println!("-- sw_vers --");
    println!("{}", run("sw_vers", &[]).out);

    println!("-- `launchctl help` (every subcommand available on this OS) --");
    let h = run("launchctl", &["help"]);
    println!("exit={}\n{}{}", h.code, h.out, h.err);

    for sub in ["kickstart", "print", "bootstrap", "attach", "blame", "procinfo", "wait4path"] {
        let r = run("launchctl", &["help", sub]);
        println!("-- launchctl help {sub}: exit={} --\n{}{}", r.code, r.out.trim(), r.err.trim());
    }

    println!("\n-- man launchctl, grepped for anything blocking/waiting --");
    let m = run("sh", &["-c", "man 1 launchctl 2>/dev/null | col -b | grep -n -i 'wait\\|block\\|synchron\\|notif\\|-p ' | head -40"]);
    println!("{}{}", m.out, m.err);

    println!("\n-- man launchctl: the kickstart section, verbatim --");
    let mk = run("sh", &["-c", "man 1 launchctl 2>/dev/null | col -b | sed -n '/^     kickstart/,/^     attach/p'"]);
    println!("{}{}", mk.out, mk.err);
    println!("-- man launchctl: the `start` (legacy) section, verbatim --");
    let ms = run("sh", &["-c", "man 1 launchctl 2>/dev/null | col -b | sed -n '/^     start service-name/,/^     setenv/p'"]);
    println!("{}{}", ms.out, ms.err);

    // What is the full vocabulary of `launchctl print`'s `state = ` field?
    // The real fix has to classify every *starting* state, not just the one
    // this probe happened to catch.
    println!("\n-- `state = ` vocabulary: strings in /sbin/launchd near \"xpcproxy\" --");
    let all = strings_of("/sbin/launchd");
    if let Some(i) = all.iter().position(|s| s == "xpcproxy") {
        let lo = i.saturating_sub(60);
        let hi = (i + 60).min(all.len());
        for (j, s) in all[lo..hi].iter().enumerate() {
            println!("  [{}]{} {s:?}", lo + j, if lo + j == i { " <<<" } else { "" });
        }
    } else {
        println!("  \"xpcproxy\" not found as a standalone literal");
    }
    for probe in ["running", "not running", "waiting", "spawn scheduled", "exited", "stopping", "trampoline"] {
        println!("  literal {probe:?} present in /sbin/launchd: {}", all.iter().any(|s| s == probe));
    }

    println!("\n-- ServiceManagement / SMAppService availability in the SDK --");
    let sdk = run("xcrun", &["--show-sdk-path"]);
    println!("sdk: {}", sdk.out.trim());
    let sm = run("sh", &["-c", &format!(
        "ls -d '{}'/System/Library/Frameworks/ServiceManagement.framework 2>&1; \
         grep -rl 'SMAppService\\|SMJobSubmit\\|SMJobBless' '{}'/System/Library/Frameworks/ServiceManagement.framework/Headers 2>&1 | head",
        sdk.out.trim(), sdk.out.trim())]);
    println!("{}{}", sm.out, sm.err);
    let smh = run("sh", &["-c", &format!(
        "cat '{}'/System/Library/Frameworks/ServiceManagement.framework/Headers/*.h 2>/dev/null | grep -n 'wait\\|status\\|register\\|API_AVAIL\\|typedef NS_ENUM' | head -40",
        sdk.out.trim())]);
    println!("{}", smh.out);

    println!("\n-- libxpc: is there a public 'wait for job' entry point? --");
    let xp = run("sh", &["-c", &format!(
        "grep -rn 'xpc_' '{}'/usr/include/xpc/*.h 2>/dev/null | grep -i 'launch\\|job\\|service\\|activat' | head -30",
        sdk.out.trim())]);
    println!("{}", xp.out);

    // -- NOTE_TRACK on launchd (pid 1): can we arm *before* the spawn? -----
    println!("\n-- EVFILT_PROC/NOTE_TRACK on launchd (pid 1): arm before the spawn, catch NOTE_CHILD --");
    let path = write_plist(LABEL, &["/bin/sleep", "300"]);
    let target = format!("system/{LABEL}");
    run("launchctl", &["bootout", &target]);
    run("launchctl", &["bootstrap", "system", &path]);

    let kq = Kq::new();
    let reg = kq.watch(1, NOTE_TRACK | NOTE_FORK | NOTE_EXEC);
    println!("register NOTE_TRACK|NOTE_FORK|NOTE_EXEC on pid 1 -> {reg:?}");
    if reg.is_ok() {
        let t = Instant::now();
        let k = run("launchctl", &["kickstart", "-p", &target]);
        println!("kickstart -p out={:?}", k.out.trim());
        let mut n = 0;
        while let Some(f) = kq.wait(Duration::from_millis(1500)) {
            println!("  event from pid-1 track: {} at {:.0}us", fflag_names(f), t.elapsed().as_secs_f64() * 1e6);
            n += 1;
            if n > 20 {
                break;
            }
        }
        if n == 0 {
            println!("  no events -- NOTE_TRACK on pid 1 delivers nothing (launchd does not fork(2) to spawn jobs, or tracking pid 1 is denied)");
        }
    }
    run("launchctl", &["bootout", &target]);

    // -- does a job's own exit show up via `launchctl print`? --------------
    println!("\n-- `launchctl print system` top-level: does it have a generation counter we could watch? --");
    let p = run("sh", &["-c", "launchctl print system | head -40"]);
    println!("{}", p.out);

    let _ = std::fs::remove_file(&path);
}

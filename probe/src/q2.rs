use crate::util::*;

const LABEL: &str = "com.goetia.probe.kickpid";

pub fn run_q2() {
    hdr("Q2: does `launchctl kickstart -p` return the PID?");
    let path = write_plist(LABEL, &["/bin/sleep", "300"]);
    let target = format!("system/{LABEL}");
    run("launchctl", &["bootout", &target]);

    for (name, args) in [
        ("kickstart (no flags)", vec!["kickstart", target.as_str()]),
        ("kickstart -p", vec!["kickstart", "-p", target.as_str()]),
        ("kickstart -kp", vec!["kickstart", "-kp", target.as_str()]),
    ] {
        run("launchctl", &["bootout", &target]);
        let b = run("launchctl", &["bootstrap", "system", &path]);
        println!("[bootstrap] code={} err={:?}", b.code, b.err.trim());
        let r = run("launchctl", &args);
        println!(
            "{name}: exit={} took={:.1}ms\n  stdout bytes = {:?}\n  stderr bytes = {:?}",
            r.code,
            r.took.as_secs_f64() * 1e3,
            r.out,
            r.err
        );
        let after = run("launchctl", &["print", &target]);
        println!("  print right after: {:?}", parse_print(&after.out));
    }

    println!("\n-- `launchctl help kickstart` / usage --");
    let h = run("launchctl", &["kickstart"]);
    println!("bare `launchctl kickstart`: exit={} out={:?} err={:?}", h.code, h.out.trim(), h.err.trim());

    run("launchctl", &["bootout", &target]);
    let _ = std::fs::remove_file(&path);
}

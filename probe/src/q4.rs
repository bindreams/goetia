use crate::util::*;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::time::{Duration, Instant};

extern "C" {
    fn notify_register_file_descriptor(name: *const libc::c_char, notify_fd: *mut i32, flags: i32, out_token: *mut i32) -> u32;
    fn notify_post(name: *const libc::c_char) -> u32;
}

const NOTIFY_REUSE: i32 = 0x8;
const LABEL: &str = "com.goetia.probe.notify";

struct Watch {
    fd: i32,
    by_token: BTreeMap<i32, String>,
}

impl Watch {
    fn new() -> Self {
        Watch { fd: -1, by_token: BTreeMap::new() }
    }

    fn add(&mut self, key: &str) -> Result<(), u32> {
        let Ok(c) = CString::new(key) else { return Err(u32::MAX) };
        let mut token = 0i32;
        let first = self.fd == -1;
        let mut fd = self.fd;
        let st = unsafe { notify_register_file_descriptor(c.as_ptr(), &mut fd, if first { 0 } else { NOTIFY_REUSE }, &mut token) };
        if st != 0 {
            return Err(st);
        }
        self.fd = fd;
        self.by_token.insert(token, key.to_string());
        Ok(())
    }

    /// Drains every buffered notification, blocking up to `bound` for the first.
    fn drain(&self, bound: Duration) -> Vec<(String, Duration)> {
        let t0 = Instant::now();
        let mut hits = Vec::new();
        let mut budget = bound;
        loop {
            let mut pfd = libc::pollfd { fd: self.fd, events: libc::POLLIN, revents: 0 };
            let n = unsafe { libc::poll(&mut pfd, 1, budget.as_millis() as i32) };
            if n <= 0 {
                return hits;
            }
            let mut buf = [0u8; 4];
            let r = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, 4) };
            if r != 4 {
                return hits;
            }
            let token = i32::from_be_bytes(buf);
            hits.push((self.by_token.get(&token).cloned().unwrap_or_else(|| format!("<token {token}>")), t0.elapsed()));
            budget = Duration::from_millis(250);
        }
    }
}

pub fn run_q4() {
    hdr("Q4: Darwin notify(3)");

    // -- self-test: prove the fd machinery works at all -------------------
    {
        let mut w = Watch::new();
        let key = "com.goetia.probe.selftest";
        match w.add(key) {
            Ok(()) => {
                let c = CString::new(key).unwrap();
                unsafe { notify_post(c.as_ptr()) };
                let hits = w.drain(Duration::from_secs(2));
                println!("self-test (post our own key): fired = {hits:?}");
                if hits.is_empty() {
                    println!("!!! self-test FAILED -- every negative result below is meaningless");
                }
            }
            Err(e) => println!("!!! self-test registration failed: status={e}"),
        }
    }

    // -- documented public keys -------------------------------------------
    println!("\n-- documented public notify keys (notify_keys.h) --");
    let sdk = run("xcrun", &["--show-sdk-path"]);
    let mut header_paths = vec!["/usr/include/notify_keys.h".to_string()];
    if sdk.code == 0 {
        header_paths.push(format!("{}/usr/include/notify_keys.h", sdk.out.trim()));
    }
    let mut found_header = false;
    for p in &header_paths {
        if let Ok(text) = std::fs::read_to_string(p) {
            found_header = true;
            println!("[{p}]");
            for l in text.lines().filter(|l| l.contains("#define") || l.contains("com.apple")) {
                println!("  {l}");
            }
            break;
        }
    }
    if !found_header {
        println!("  notify_keys.h not present at {header_paths:?}");
    }
    println!("  man notify(3) key list:");
    let m = run("sh", &["-c", "man 3 notify 2>/dev/null | col -b | grep -i -n 'key\\|launchd' | head -40"]);
    println!("{}", m.out);

    // -- harvest candidate keys from the binaries themselves ---------------
    let mut cands: Vec<String> = Vec::new();
    for bin in ["/sbin/launchd", "/bin/launchctl", "/usr/lib/system/libsystem_notify.dylib"] {
        let s = strings_of(bin);
        let mut n = 0;
        for t in s {
            if t.starts_with("com.apple.") && t.len() < 80 && !t.contains(' ') && !t.contains('%') && !cands.contains(&t) {
                cands.push(t);
                n += 1;
            }
        }
        println!("\n{bin}: {n} distinct `com.apple.*` string literals");
    }
    for extra in [
        "com.apple.launchd.jobs",
        "com.apple.launchd.job",
        "com.apple.launchd",
        "com.apple.launchctl",
        "com.apple.system.launchd",
        "com.apple.xpc.launchd",
        "com.apple.bootstrap",
        "com.apple.launchd.system",
    ] {
        if !cands.iter().any(|c| c == extra) {
            cands.push(extra.to_string());
        }
    }
    println!("candidate keys to register: {}", cands.len());
    let launchdish: Vec<&String> = cands.iter().filter(|c| c.contains("launch") || c.contains("job") || c.contains("service") || c.contains("xpc")).collect();
    println!("of which launchd-ish ({}): {:?}", launchdish.len(), launchdish);

    let mut w = Watch::new();
    let mut regfail = 0;
    for c in &cands {
        if w.add(c).is_err() {
            regfail += 1;
        }
    }
    println!("registered {} keys ({regfail} failed)", w.by_token.len());

    // -- perform a full job lifecycle and see what posts -------------------
    let path = write_plist(LABEL, &["/bin/sleep", "300"]);
    let target = format!("system/{LABEL}");
    run("launchctl", &["bootout", &target]);
    w.drain(Duration::from_millis(200));

    let b = run("launchctl", &["bootstrap", "system", &path]);
    println!("\n[bootstrap exit={}] notifications: {:?}", b.code, w.drain(Duration::from_secs(2)));
    let k = run("launchctl", &["kickstart", "-p", &target]);
    println!("[kickstart exit={} out={:?}] notifications: {:?}", k.code, k.out.trim(), w.drain(Duration::from_secs(2)));
    let x = run("launchctl", &["bootout", &target]);
    println!("[bootout exit={}] notifications: {:?}", x.code, w.drain(Duration::from_secs(2)));

    // -- notifyutil ---------------------------------------------------------
    let nu = run("sh", &["-c", "command -v notifyutil"]);
    println!("\nnotifyutil present: {:?}", nu.out.trim());

    let _ = std::fs::remove_file(&path);
}

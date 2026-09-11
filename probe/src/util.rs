use std::process::Command;
use std::time::{Duration, Instant};

pub struct Run {
    pub code: i32,
    pub out: String,
    pub err: String,
    pub took: Duration,
}

pub fn run(cmd: &str, args: &[&str]) -> Run {
    let t0 = Instant::now();
    let o = Command::new(cmd).args(args).output();
    let took = t0.elapsed();
    match o {
        Ok(o) => Run {
            code: o.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&o.stdout).into_owned(),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
            took,
        },
        Err(e) => Run { code: -999, out: String::new(), err: e.to_string(), took },
    }
}

pub fn hdr(s: &str) {
    println!("\n======== {s} ========");
}

/// min / median / p95 / p99 / max, in microseconds.
pub fn stats(name: &str, v: &mut Vec<Duration>) {
    if v.is_empty() {
        println!("{name}: (no samples)");
        return;
    }
    v.sort();
    let us = |d: &Duration| d.as_secs_f64() * 1e6;
    let at = |q: f64| {
        let i = ((v.len() as f64 - 1.0) * q).round() as usize;
        us(&v[i])
    };
    let mean = v.iter().map(us).sum::<f64>() / v.len() as f64;
    println!(
        "{name}: n={} min={:.0}us p50={:.0}us p95={:.0}us p99={:.0}us max={:.0}us mean={:.0}us",
        v.len(),
        us(&v[0]),
        at(0.50),
        at(0.95),
        at(0.99),
        us(&v[v.len() - 1]),
        mean
    );
}

pub fn pid_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// `launchctl print`'s `state = ...` and `pid = ...` fields, the exact two
/// goetia's `query_live_state` reads.
pub fn parse_print(text: &str) -> (Option<String>, Option<i32>) {
    let field = |key: &str| -> Option<String> {
        let prefix = format!("{key} = ");
        text.lines()
            .find_map(|l| l.trim_start().strip_prefix(prefix.as_str()))
            .map(|s| s.trim().to_string())
    };
    (field("state"), field("pid").and_then(|s| s.parse().ok()))
}

/// `launchctl list <label>`'s legacy plist-ish body: `"PID" = 1234;`
pub fn parse_list_pid(text: &str) -> Option<i32> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix("\"PID\" = "))
        .and_then(|s| s.trim_end_matches(';').trim().parse().ok())
}

pub fn write_plist(label: &str, argv: &[&str]) -> String {
    let path = format!("/Library/LaunchDaemons/{label}.plist");
    let mut args = String::new();
    for a in argv {
        args.push_str(&format!("    <string>{a}</string>\n"));
    }
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\n\
         <key>Label</key><string>{label}</string>\n\
         <key>ProgramArguments</key><array>\n{args}</array>\n\
         </dict></plist>\n"
    );
    std::fs::write(&path, body).expect("write plist");
    path
}

/// Printable ASCII runs of length >= 6 in a file, deduplicated, in order.
pub fn strings_of(path: &str) -> Vec<String> {
    let Ok(bytes) = std::fs::read(path) else { return Vec::new() };
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for &b in &bytes {
        if (0x20..0x7f).contains(&b) {
            cur.push(b);
        } else {
            if cur.len() >= 6 {
                out.push(String::from_utf8_lossy(&cur).into_owned());
            }
            cur.clear();
        }
    }
    if cur.len() >= 6 {
        out.push(String::from_utf8_lossy(&cur).into_owned());
    }
    out
}

//! Step 0: the measurements that gate owner questions. Throwaway; never merged. Every
//! system-affecting subcommand refuses to run off a GitHub-hosted macOS runner.

mod b0;
mod common;
mod devices;
mod gap;
mod launch;
mod nfs;
mod size;
mod tcc;
mod teardown;

use std::io::Write;
use std::path::Path;

use serde_json::{json, Value};

use common::*;
use launch::JobSpec;

/// Subcommands that only read or probe one path as the calling account run without guards.
fn helper(args: &[String]) -> i32 {
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or("");
    match arg(0) {
        "lookup" => {
            println!("pid={}", std::process::id());
            let _ = std::io::stdout().flush();
            match std::fs::read_dir(arg(1)) {
                Ok(it) => println!("entries={}", it.count()),
                Err(e) => println!("errno={}", e.raw_os_error().unwrap_or(-1)),
            }
            0
        }
        "access" => {
            println!(
                "errno={}",
                crate::sys::access(Path::new(arg(2)), arg(1).parse().unwrap_or(0))
            );
            0
        }
        "search-stat" => {
            const O_SEARCH: i32 = 0x4000_0000 | 0x0010_0000;
            let c = crate::sys::cpath(Path::new(arg(1)));
            let fd = unsafe { libc::open(c.as_ptr(), O_SEARCH) };
            if fd < 0 {
                println!("open={}", crate::sys::errno());
                return 0;
            }
            println!("open=0");
            let n = crate::sys::cstr(arg(2));
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            let e = if unsafe { libc::fstatat(fd, n.as_ptr(), &mut st, 0) } == 0 {
                0
            } else {
                crate::sys::errno()
            };
            println!("fstatat={e}");
            0
        }
        "stall" => {
            // `pre` first, then a stat of the mount root and of an uncached name (a LOOKUP RPC).
            println!("pre");
            let _ = std::io::stdout().flush();
            let root = arg(1);
            println!("root={}", std::fs::symlink_metadata(root).is_ok());
            let _ = std::io::stdout().flush();
            println!(
                "miss={}",
                std::fs::symlink_metadata(Path::new(root).join("no-such-name")).is_ok()
            );
            0
        }
        other => {
            eprintln!("unknown helper {other}");
            2
        }
    }
}

pub fn main(args: &[String]) -> i32 {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let arg = args.get(1).map(String::as_str).unwrap_or("");
    if sub == "helper" {
        return helper(&args[1..]);
    }
    let mut ctx = match Ctx::init() {
        Ok(c) => c,
        Err(code) => return code,
    };
    match sub {
        "runner-info" => runner_info(&mut ctx),
        "selftest" => selftest(&mut ctx),
        "selfcheck-verdicts" => selfcheck_verdicts(&mut ctx),
        "b1" => b0::b1(&mut ctx),
        "b2" => b0::b2(&mut ctx),
        "b3" => b0::b3(&mut ctx),
        "b4" => b0::b4(&mut ctx),
        "b5" => b0::b5(&mut ctx),
        "b6" => size::b6(&mut ctx),
        "devices" => devices::run(&mut ctx, arg),
        "tcc-setup" => tcc::setup(&mut ctx),
        "tcc" => tcc::dir_step(&mut ctx, arg),
        "tcc-removable" => tcc::removable(&mut ctx),
        "tcc-t2" => tcc::t2(&mut ctx),
        "nfs-setup" => nfs::setup(&mut ctx),
        "n1" => nfs::n1(&mut ctx),
        "n2" => nfs::n2(&mut ctx),
        "n3" => nfs::n3(&mut ctx),
        "n4" => nfs::n4(&mut ctx),
        "m12" => nfs::m12(&mut ctx),
        "m3" => nfs::m3(&mut ctx),
        "gap" => gap::run(&mut ctx),
        "teardown" => return teardown::run(&ctx),
        other => {
            eprintln!("unknown step0 subcommand {other:?}");
            return 2;
        }
    }
    ctx.finish()
}

fn runner_info(ctx: &mut Ctx) {
    let mut r = Res::new("RUNNER", "info", &[]);
    r.observed = json!({
        "sw_vers": sh("sw_vers"), "uname_v": sh("uname -v").trim(), "csrutil": sh("csrutil status").trim(),
        "hw_model": sh("sysctl -n hw.model").trim(), "df": sh("df -h /"), "id": sh("id").trim(),
        "image_version": std::env::var("ImageVersion").ok(), "mount": sh("mount"),
    });
    ctx.emit(r);
}

/// The earlier probe's selftest (zombie child, reparented grandchild, exec), unchanged.
fn selftest(ctx: &mut Ctx) {
    let mut r = Res::new("SELF.selftest", "self", &[]);
    let code = crate::selftest();
    let doc: Value = std::fs::read_to_string("results/selftest.json")
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let _ = std::fs::remove_file("results/selftest.json");
    r.observed = doc;
    if code != 0 {
        r.anomaly("selftest failed");
    }
    ctx.emit(r);
}

/// The verdict machinery against known answers: a good sentinel is Ran, a nonexistent cwd is
/// Refused with exit 78.
fn selfcheck_verdicts(ctx: &mut Ctx) {
    let ld = Path::new("/Library/LaunchDaemons");
    let mut good = Res::new("SELF.verdict-ran", "self", &[]).expect(json!("ran"));
    let o = launch::launch(ctx, &JobSpec::new("self-ran", "nobody", ld));
    o.apply(&mut good);
    if o.verdict != "ran" {
        good.anomaly(format!("known-good sentinel is {}", o.verdict));
    }
    good.observed = o.detail();
    ctx.emit(good);

    let mut bad = Res::new("SELF.verdict-refused", "self", &[]).expect(json!("refused, exit 78"));
    let mut spec = JobSpec::new("self-refused", "nobody", ld);
    spec.cwd = Some(format!("/nonexistent-goetia-probe-{}", ctx.run).into());
    let o = launch::launch(ctx, &spec);
    o.apply(&mut bad);
    if o.verdict != "refused" {
        bad.anomaly(format!("nonexistent cwd is {}, want refused", o.verdict));
    }
    bad.observed = o.detail();
    ctx.emit(bad);
}

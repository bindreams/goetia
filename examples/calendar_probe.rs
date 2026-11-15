//! Probe only: installs a task through goetia's launchd backend and prints goetia's `next_run`.
//! Usage (as root): calendar_probe install <id> "<cron>" <command...> | next <id> | enable <id> | disable <id> | uninstall <id>

use std::time::UNIX_EPOCH;

use goetia::backend::launchd::manager::Launchd;
use goetia::manager::{ServiceManager, Status};
use goetia::spec::Id;
use goetia::testing::task;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mgr = Launchd::new();
    let id = Id::try_from(args[1].as_str()).expect("id");
    match args[0].as_str() {
        "install" => {
            let spec = task(&args[1], args[3..].to_vec(), Some(&args[2]));
            mgr.install(&spec, false).expect("install");
            mgr.enable(&id).expect("enable");
            print_next(&mgr, &id);
        }
        "next" => print_next(&mgr, &id),
        "enable" => {
            mgr.enable(&id).expect("enable");
            print_next(&mgr, &id);
        }
        "disable" => mgr.disable(&id).expect("disable"),
        "uninstall" => mgr.uninstall(&id).expect("uninstall"),
        other => panic!("unknown verb {other}"),
    }
}

fn print_next(mgr: &Launchd, id: &Id) {
    let Status::Task(status) = mgr.status(id).expect("status") else {
        panic!("not a task");
    };
    match status.next_run {
        Some(at) => println!("GOETIA_NEXT_RUN {}", at.duration_since(UNIX_EPOCH).unwrap().as_secs()),
        None => println!("GOETIA_NEXT_RUN none"),
    }
}

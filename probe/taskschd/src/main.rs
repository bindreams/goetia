// tsprobe: the proof program behind the Task Scheduler measurements L1-L3 and M1-M11 (goetia slice 5).
// It touches Task Scheduler and accounts: run it only on a throwaway Windows machine (a CI runner).
//
//   tsprobe act <mode> ...        what a task's action does (rec, hold, ctrl, ctrlchild, self, prio, noop)
//   tsprobe ended <task> ...      L1-L3: sample ended instances; hooks at the first exited-but-listed sample
//   tsprobe nostart <task> <n>    an action that cannot start: what is listed
//   tsprobe runtimes <task> ...   M2: GetRunTimes over a window
//   tsprobe handles <task> ...    M5: who holds a handle to the action's process
//
// Every wait here is on a handle, a listing read or an event; there are no sleeps.
#![allow(clippy::all)]
mod act;
mod ended;
mod handles;
mod runtimes;
mod util;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 2 {
        eprintln!("usage: tsprobe act|ended|nostart|runtimes|handles ...");
        std::process::exit(2);
    }
    let code = match a[1].as_str() {
        "act" => act::run(&a[2..]),
        "ended" => ended::run(&a[2..]),
        "nostart" => ended::nostart(&a[2..]),
        "runtimes" => runtimes::run(&a[2..]),
        "handles" => handles::run(&a[2..]),
        other => {
            eprintln!("unknown subcommand {other}");
            2
        }
    };
    std::process::exit(code);
}

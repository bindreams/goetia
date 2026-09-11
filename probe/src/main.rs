//! Throwaway spike. Measures how a launchd job's start can be observed
//! without polling. Delete with the branch.

mod q1;
mod q2;
mod q3;
mod q4;
mod q5;
mod util;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("all");
    let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(200);
    match cmd {
        "q1" => {
            q1::run_q1(n, true);
            q1::run_q1(n, false);
        }
        "q2" => q2::run_q2(),
        "q3" => q3::run_q3(),
        "q3-unelevated" => q3::run_q3_unelevated(n as i32),
        "q4" => q4::run_q4(),
        "q5" => q5::run_q5(),
        "all" => {
            q2::run_q2();
            q3::run_q3();
            q4::run_q4();
            q5::run_q5();
            q1::run_q1(n, true);
            q1::run_q1(n, false);
        }
        other => {
            eprintln!("unknown: {other}");
            std::process::exit(2);
        }
    }
}

// M2. `runtimes <task> <start YYYY-MM-DD> <end YYYY-MM-DD> [cap]`   or   `runtimes <task> captest`
//
// IRegisteredTask::GetRunTimes over [start, end), paged by the last time returned (PowerShell cannot call it: KB).
// Prints a summary: count, first and last times, the distinct times of day, the weekdays, and the days of each month.
use crate::util::*;
use std::collections::{BTreeMap, BTreeSet};
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::TaskScheduler::IRegisteredTask;
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToFileTime};

fn st(date: &str) -> SYSTEMTIME {
    let p: Vec<u16> = date.split('-').map(|x| x.parse().unwrap()).collect();
    SYSTEMTIME { wYear: p[0], wMonth: p[1], wDayOfWeek: 0, wDay: p[2], wHour: 0, wMinute: 0, wSecond: 0, wMilliseconds: 0 }
}

fn plus_secs(s: &SYSTEMTIME, secs: u64) -> SYSTEMTIME {
    // SAFETY: plain conversions on live structs.
    unsafe {
        let mut ft = FILETIME::default();
        let _ = SystemTimeToFileTime(s, &mut ft);
        let v = ft_u64(ft) + secs * 10_000_000;
        let ft2 = FILETIME { dwLowDateTime: v as u32, dwHighDateTime: (v >> 32) as u32 };
        let mut o = SYSTEMTIME::default();
        let _ = FileTimeToSystemTime(&ft2, &mut o);
        o
    }
}

fn dow(y: u16, m: u16, d: u16) -> u16 {
    let t = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if m < 3 { y - 1 } else { y } as i32;
    ((y + y / 4 - y / 100 + y / 400 + t[(m - 1) as usize] + d as i32) % 7) as u16
}

fn fmt(s: &SYSTEMTIME) -> String {
    if s.wSecond == 0 {
        format!("{:04}-{:02}-{:02} {:02}:{:02}", s.wYear, s.wMonth, s.wDay, s.wHour, s.wMinute)
    } else {
        format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", s.wYear, s.wMonth, s.wDay, s.wHour, s.wMinute, s.wSecond)
    }
}

fn call(t: &IRegisteredTask, start: &SYSTEMTIME, end: &SYSTEMTIME, cap: u32) -> std::result::Result<Vec<SYSTEMTIME>, String> {
    let mut cnt = cap;
    let mut ptr: *mut SYSTEMTIME = std::ptr::null_mut();
    // SAFETY: the structs are live; a returned block is copied out and freed with CoTaskMemFree.
    unsafe {
        t.GetRunTimes(start, end, &mut cnt, &mut ptr).map_err(|e| hr(&e))?;
        let v = if ptr.is_null() { Vec::new() } else { std::slice::from_raw_parts(ptr, cnt as usize).to_vec() };
        if !ptr.is_null() {
            CoTaskMemFree(Some(ptr.cast()));
        }
        Ok(v)
    }
}

fn ranges(days: &BTreeSet<u16>) -> String {
    let v: Vec<u16> = days.iter().copied().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let mut j = i;
        while j + 1 < v.len() && v[j + 1] == v[j] + 1 {
            j += 1;
        }
        out.push(if j > i + 1 { format!("{}-{}", v[i], v[j]) } else if j == i + 1 { format!("{},{}", v[i], v[j]) } else { format!("{}", v[i]) });
        i = j + 1;
    }
    out.join(",")
}

pub fn run(a: &[String]) -> i32 {
    com_init();
    let t = match task(&a[0]) {
        Ok(t) => t,
        Err(e) => {
            println!("task {} unreadable {}", a[0], hr(&e));
            return 1;
        }
    };
    if a.get(1).map(String::as_str) == Some("captest") {
        let (s, e) = (st("2027-01-01"), st("2029-01-01"));
        for cap in [0u32, 1, 10, 1440, 100_000, u32::MAX] {
            match call(&t, &s, &e, cap) {
                Ok(v) => println!("captest cap_in={cap} returned={} last={}", v.len(), v.last().map(fmt).unwrap_or_default()),
                Err(h) => println!("captest cap_in={cap} err {h}"),
            }
        }
        return 0;
    }
    let (start, end) = (st(&a[1]), st(&a[2]));
    let cap: u32 = a.get(3).and_then(|c| c.parse().ok()).unwrap_or(100_000);
    let mut all: Vec<SYSTEMTIME> = Vec::new();
    let mut cur = start;
    loop {
        match call(&t, &cur, &end, cap) {
            Err(h) => {
                println!("GetRunTimes err {h}");
                break;
            }
            Ok(v) => {
                if v.is_empty() {
                    break;
                }
                cur = plus_secs(v.last().unwrap(), 1);
                all.extend(v);
            }
        }
    }
    if std::env::var_os("TSPROBE_ALL").is_some() {
        for s in &all {
            println!("t {}", fmt(s));
        }
    }
    println!("count={} window=[{}, {})", all.len(), a[1], a[2]);
    if all.is_empty() {
        return 0;
    }
    println!("first: {}", all.iter().take(6).map(fmt).collect::<Vec<_>>().join(" | "));
    println!("last: {}", all.iter().rev().take(3).rev().map(fmt).collect::<Vec<_>>().join(" | "));
    let tods: BTreeSet<String> = all.iter().map(|s| format!("{:02}:{:02}", s.wHour, s.wMinute)).collect();
    if tods.len() <= 40 {
        println!("times-of-day ({}): {}", tods.len(), tods.iter().cloned().collect::<Vec<_>>().join(" "));
    } else {
        println!("times-of-day: {} distinct, from {} to {}", tods.len(), tods.iter().next().unwrap(), tods.iter().next_back().unwrap());
    }
    let mut wd = BTreeMap::<u16, u32>::new();
    let mut months = BTreeMap::<(u16, u16), BTreeSet<u16>>::new();
    let mut dup = 0;
    for w in all.windows(2) {
        if fmt(&w[0]) == fmt(&w[1]) {
            dup += 1;
        }
    }
    for s in &all {
        *wd.entry(dow(s.wYear, s.wMonth, s.wDay)).or_default() += 1;
        months.entry((s.wYear, s.wMonth)).or_default().insert(s.wDay);
    }
    println!("weekdays (0=Sun) -> count: {wd:?}  duplicates-adjacent={dup}");
    for ((y, m), d) in &months {
        println!("days {y}-{m:02}: {}", ranges(d));
    }
    0
}

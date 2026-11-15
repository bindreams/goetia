use std::io::Write;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows::Win32::System::TaskScheduler::*;
use windows::Win32::System::Threading::*;
use windows::Win32::System::Variant::VARIANT;

pub fn now_ft() -> u64 {
    // SAFETY: plain call, no arguments.
    let ft = unsafe { GetSystemTimePreciseAsFileTime() };
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

pub fn ft_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

/// Milliseconds between two FILETIMEs (100 ns ticks), signed.
pub fn ms(from: u64, to: u64) -> f64 {
    (to as f64 - from as f64) / 10_000.0
}

pub fn hr(e: &Error) -> String {
    format!("0x{:08X}", e.code().0 as u32)
}

pub fn com_init() {
    // SAFETY: once per thread, before any COM call on it.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok().expect("CoInitializeEx");
    }
}

pub fn connect() -> Result<ITaskService> {
    // SAFETY: COM is initialised by the caller.
    unsafe {
        let svc: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
        let v = VARIANT::default();
        svc.Connect(&v, &v, &v, &v)?;
        Ok(svc)
    }
}

pub fn task(path: &str) -> Result<IRegisteredTask> {
    let svc = connect()?;
    let (folder, name) = path.rsplit_once('\\').expect("task path with a folder");
    let folder = if folder.is_empty() { "\\" } else { folder };
    // SAFETY: plain COM calls.
    unsafe { svc.GetFolder(&BSTR::from(folder))?.GetTask(&BSTR::from(name)) }
}

pub struct Inst {
    pub it: IRunningTask,
    pub guid: String,
    pub pid: u32,
}

/// The listing: every instance with its guid and EnginePID. An error in the middle is returned whole.
pub fn instances(t: &IRegisteredTask) -> Result<Vec<Inst>> {
    // SAFETY: plain COM calls.
    unsafe {
        let coll = t.GetInstances(0)?;
        let n = coll.Count()?;
        let mut out = Vec::new();
        for i in 1..=n {
            let it = coll.get_Item(&VARIANT::from(i))?;
            let guid = it.InstanceGuid()?.to_string();
            let pid = it.EnginePID()?;
            out.push(Inst { it, guid, pid });
        }
        Ok(out)
    }
}

pub fn same_guid(a: &str, b: &str) -> bool {
    a.trim_matches(|c| c == '{' || c == '}').eq_ignore_ascii_case(b.trim_matches(|c| c == '{' || c == '}'))
}

#[derive(Clone, Copy, Debug)]
pub struct PInfo {
    pub ct: u64,
    pub exit_ft: u64,
    pub code: u32,
    pub exited: bool,
}

/// Opens `pid` as `kill.rs` does (query limited + synchronize), reads creation time, exit time, exit code and
/// whether the process object is signalled, and closes the handle again. Err is the Win32 error of the open.
pub fn probe_pid(pid: u32) -> std::result::Result<PInfo, u32> {
    // SAFETY: the handle is closed before return; the out structs are live.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, false, pid)
            .map_err(|e| (e.code().0 as u32) & 0xFFFF)?;
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let _ = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
        let mut code = 0u32;
        let _ = GetExitCodeProcess(h, &mut code);
        let exited = WaitForSingleObject(h, 0) == WAIT_OBJECT_0;
        let _ = CloseHandle(h);
        Ok(PInfo { ct: ft_u64(c), exit_ft: ft_u64(e), code, exited })
    }
}

pub fn image_of(pid: u32) -> String {
    // SAFETY: the handle is closed before return; the buffer is live.
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return "?".into();
        };
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        if r.is_ok() {
            String::from_utf16_lossy(&buf[..len as usize])
        } else {
            "?".into()
        }
    }
}

pub fn append(path: &str, line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

pub fn lines(path: &str) -> Vec<String> {
    std::fs::read_to_string(path).map(|s| s.lines().map(str::to_owned).collect()).unwrap_or_default()
}

/// `key=value` out of a log line.
pub fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace().find_map(|w| w.strip_prefix(key)?.strip_prefix('='))
}

pub fn ole_date(d: f64) -> String {
    format!("{d}")
}

pub fn stats(mut v: Vec<f64>) -> String {
    if v.is_empty() {
        return "n=0".into();
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    let p = |q: f64| v[(((n - 1) as f64) * q).round() as usize];
    format!("n={n} min={:.2} median={:.2} p90={:.2} p99={:.2} max={:.2}", v[0], p(0.5), p(0.9), p(0.99), v[n - 1])
}

pub fn self_ct() -> u64 {
    // SAFETY: the pseudo-handle is always valid.
    unsafe {
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
        ft_u64(c)
    }
}

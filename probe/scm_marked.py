"""Throwaway measurement (never merged): how does the Windows SCM present a
service that is marked for deletion while another handle holds it open?

Questions (one line of evidence each, printed as `Q<n>: ...`):
  Q1  EnumServicesStatusExW lists the marked service?
  Q2  OpenServiceW on the marked service: success or which error?
  Q3  GetServiceDisplayNameW on the marked service?
  Q4  CreateServiceW with the same name while marked?
  Q5  Registry key Services\\<name> exists while marked? DeleteFlag value?
  Q6  ControlService(STOP) through a handle obtained before DeleteService
      (stopped service)?
  Q7  SERVICE_NOTIFY_DELETED on the SCM handle arrives after the last handle
      closes, when the holder is (a) a child process, (b) this process?
  Q8  At the moment the DELETED notification is delivered: key present?
      enumeration still lists it?
  Q10 QueueUserAPC accepts the GetCurrentThread pseudo handle?

Waits on the SCM notification are bounded by FAIL_BOUND_MS: that's the
human-facing failure bound on an external event (the SCM), not a sync.
"""

import ctypes
import ctypes.wintypes as w
import subprocess
import sys
import winreg

FAIL_BOUND_MS = 60_000

advapi = ctypes.WinDLL("advapi32", use_last_error=True)
kernel = ctypes.WinDLL("kernel32", use_last_error=True)

SC_MANAGER_ALL_ACCESS = 0xF003F
SERVICE_ALL_ACCESS = 0xF01FF
SERVICE_QUERY_STATUS = 0x0004
SERVICE_WIN32_OWN_PROCESS = 0x10
SERVICE_WIN32 = 0x30
SERVICE_DEMAND_START = 3
SERVICE_ERROR_NORMAL = 1
SERVICE_STATE_ALL = 3
SC_ENUM_PROCESS_INFO = 0
SERVICE_CONTROL_STOP = 1
SERVICE_NOTIFY_DELETED = 0x100
ERROR_MORE_DATA = 234
WAIT_IO_COMPLETION = 0xC0


class SERVICE_STATUS_PROCESS(ctypes.Structure):
    _fields_ = [(n, w.DWORD) for n in (
        "dwServiceType", "dwCurrentState", "dwControlsAccepted",
        "dwWin32ExitCode", "dwServiceSpecificExitCode", "dwCheckPoint",
        "dwWaitHint", "dwProcessId", "dwServiceFlags")]


class SERVICE_STATUS(ctypes.Structure):
    _fields_ = [(n, w.DWORD) for n in (
        "dwServiceType", "dwCurrentState", "dwControlsAccepted",
        "dwWin32ExitCode", "dwServiceSpecificExitCode", "dwCheckPoint",
        "dwWaitHint")]


class ENUM_SERVICE_STATUS_PROCESSW(ctypes.Structure):
    _fields_ = [("lpServiceName", w.LPWSTR), ("lpDisplayName", w.LPWSTR),
                ("ServiceStatusProcess", SERVICE_STATUS_PROCESS)]


NOTIFY_CB = ctypes.WINFUNCTYPE(None, ctypes.c_void_p)


class SERVICE_NOTIFY_2W(ctypes.Structure):
    _fields_ = [("dwVersion", w.DWORD), ("pfnNotifyCallback", NOTIFY_CB),
                ("pContext", ctypes.c_void_p),
                ("dwNotificationStatus", w.DWORD),
                ("ServiceStatus", SERVICE_STATUS_PROCESS),
                ("dwNotificationTriggered", w.DWORD),
                ("pszServiceNames", ctypes.c_void_p)]


H = ctypes.c_void_p
advapi.OpenSCManagerW.restype = H
advapi.OpenSCManagerW.argtypes = [w.LPCWSTR, w.LPCWSTR, w.DWORD]
advapi.OpenServiceW.restype = H
advapi.OpenServiceW.argtypes = [H, w.LPCWSTR, w.DWORD]
advapi.CreateServiceW.restype = H
advapi.CreateServiceW.argtypes = [H, w.LPCWSTR, w.LPCWSTR, w.DWORD, w.DWORD,
                                  w.DWORD, w.DWORD, w.LPCWSTR, w.LPCWSTR,
                                  ctypes.c_void_p, w.LPCWSTR, w.LPCWSTR,
                                  w.LPCWSTR]
advapi.DeleteService.argtypes = [H]
advapi.CloseServiceHandle.argtypes = [H]
advapi.ControlService.argtypes = [H, w.DWORD, ctypes.POINTER(SERVICE_STATUS)]
advapi.StartServiceW.argtypes = [H, w.DWORD, ctypes.c_void_p]
advapi.QueryServiceStatusEx.argtypes = [H, ctypes.c_int, ctypes.c_void_p,
                                        w.DWORD, ctypes.POINTER(w.DWORD)]
advapi.EnumServicesStatusExW.argtypes = [
    H, ctypes.c_int, w.DWORD, w.DWORD, ctypes.c_void_p, w.DWORD,
    ctypes.POINTER(w.DWORD), ctypes.POINTER(w.DWORD), ctypes.POINTER(w.DWORD),
    w.LPCWSTR]
advapi.GetServiceDisplayNameW.argtypes = [H, w.LPCWSTR, w.LPWSTR,
                                          ctypes.POINTER(w.DWORD)]
advapi.NotifyServiceStatusChangeW.restype = w.DWORD
advapi.NotifyServiceStatusChangeW.argtypes = [H, w.DWORD,
                                              ctypes.POINTER(SERVICE_NOTIFY_2W)]
kernel.SleepEx.restype = w.DWORD
kernel.SleepEx.argtypes = [w.DWORD, w.BOOL]
kernel.LocalFree.argtypes = [ctypes.c_void_p]
kernel.GetCurrentThread.restype = H
APC_CB = ctypes.WINFUNCTYPE(None, ctypes.c_void_p)
kernel.QueueUserAPC.argtypes = [APC_CB, H, ctypes.c_void_p]
kernel.GetTickCount64.restype = ctypes.c_uint64


def err():
    return ctypes.get_last_error()


def say(q, text):
    print(f"{q}: {text}", flush=True)


def enum_lists(scm, name):
    found = None
    resume = w.DWORD(0)
    while True:
        need = w.DWORD(0)
        count = w.DWORD(0)
        advapi.EnumServicesStatusExW(scm, SC_ENUM_PROCESS_INFO, SERVICE_WIN32,
                                     SERVICE_STATE_ALL, None, 0,
                                     ctypes.byref(need), ctypes.byref(count),
                                     ctypes.byref(resume), None)
        size = need.value
        if size == 0:
            break
        buf = ctypes.create_string_buffer(size)
        ok = advapi.EnumServicesStatusExW(
            scm, SC_ENUM_PROCESS_INFO, SERVICE_WIN32, SERVICE_STATE_ALL, buf,
            size, ctypes.byref(need), ctypes.byref(count),
            ctypes.byref(resume), None)
        e = err()
        if not ok and e != ERROR_MORE_DATA:
            return f"error {e}"
        arr = ctypes.cast(buf, ctypes.POINTER(ENUM_SERVICE_STATUS_PROCESSW))
        for i in range(count.value):
            if arr[i].lpServiceName.lower() == name.lower():
                found = arr[i].ServiceStatusProcess.dwCurrentState
        if ok:
            break
    return "not listed" if found is None else f"listed, state {found}"


def open_result(scm, name):
    h = advapi.OpenServiceW(scm, name, SERVICE_QUERY_STATUS)
    if h:
        advapi.CloseServiceHandle(h)
        return "success"
    return f"error {err()}"


def display_name(scm, name):
    buf = ctypes.create_unicode_buffer(512)
    n = w.DWORD(512)
    if advapi.GetServiceDisplayNameW(scm, name, buf, ctypes.byref(n)):
        return f"success {buf.value!r}"
    return f"error {err()}"


def key_state(name):
    path = rf"SYSTEM\CurrentControlSet\Services\{name}"
    try:
        k = winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, path)
    except FileNotFoundError:
        return "absent"
    try:
        flag = winreg.QueryValueEx(k, "DeleteFlag")[0]
    except FileNotFoundError:
        flag = "none"
    winreg.CloseKey(k)
    return f"present, DeleteFlag={flag}"


def create(scm, name, binpath):
    h = advapi.CreateServiceW(scm, name, name, SERVICE_ALL_ACCESS,
                              SERVICE_WIN32_OWN_PROCESS, SERVICE_DEMAND_START,
                              SERVICE_ERROR_NORMAL, binpath, None, None, None,
                              None, None)
    return h, (0 if h else err())


class DeletedWatch:
    """SERVICE_NOTIFY_DELETED on the SCM handle; re-arms until `name` shows up."""

    def __init__(self, scm, name):
        self.scm = scm
        self.name = name.lower()
        self.seen = False
        self.on_seen = None
        self.at_seen = None
        self.cb = NOTIFY_CB(self._cb)
        self.arm()

    def arm(self):
        self.n = SERVICE_NOTIFY_2W()
        self.n.dwVersion = 2
        self.n.pfnNotifyCallback = self.cb
        rc = advapi.NotifyServiceStatusChangeW(self.scm, SERVICE_NOTIFY_DELETED,
                                               ctypes.byref(self.n))
        if rc != 0:
            raise SystemExit(f"NotifyServiceStatusChangeW failed: {rc}")

    def _cb(self, _p):
        names = []
        if self.n.pszServiceNames:
            p = self.n.pszServiceNames
            while True:
                s = ctypes.wstring_at(p)
                if not s:
                    break
                names.append(s)
                p += (len(s) + 1) * 2
            kernel.LocalFree(self.n.pszServiceNames)
        print(f"  notify: status={self.n.dwNotificationStatus} "
              f"triggered={self.n.dwNotificationTriggered:#x} names={names!r}",
              flush=True)
        if any(s.lstrip("/").lower() == self.name for s in names):
            self.seen = True
            if self.on_seen:
                self.at_seen = self.on_seen()
        self.pending_rearm = not self.seen

    def wait(self):
        start = kernel.GetTickCount64()
        while not self.seen:
            left = FAIL_BOUND_MS - (kernel.GetTickCount64() - start)
            if left <= 0:
                return False
            self.pending_rearm = False
            if kernel.SleepEx(int(left), True) == WAIT_IO_COMPLETION and \
                    self.pending_rearm:
                self.arm()
        return True


def holder(name):
    """Child: open the service, report, wait for a line, close, exit."""
    scm = advapi.OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
    h = advapi.OpenServiceW(scm, name, SERVICE_QUERY_STATUS)
    print("held" if h else f"open failed {err()}", flush=True)
    sys.stdin.readline()
    advapi.CloseServiceHandle(h)
    advapi.CloseServiceHandle(scm)


def spawn_holder(name):
    p = subprocess.Popen([sys.executable, __file__, "hold", name],
                         stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         text=True)
    line = p.stdout.readline().strip()
    if line != "held":
        raise SystemExit(f"holder: {line!r}")
    return p


def release_holder(p):
    p.stdin.write("\n")
    p.stdin.flush()
    p.wait()


def scenario(scm, tag, binpath, holder_kind, run):
    name = f"goetia-probe-{tag}"
    print(f"== scenario {tag}: holder={holder_kind} running={run}", flush=True)
    h, e = create(scm, name, binpath)
    if not h:
        raise SystemExit(f"create {name}: {e}")
    if run:
        if not advapi.StartServiceW(h, 0, None):
            say("start", f"error {err()}")
        st = SERVICE_STATUS_PROCESS()
        need = w.DWORD(0)
        advapi.QueryServiceStatusEx(h, 0, ctypes.byref(st), ctypes.sizeof(st),
                                    ctypes.byref(need))
        say("state-before-delete", st.dwCurrentState)
    child = spawn_holder(name) if holder_kind == "child" else None
    own = advapi.OpenServiceW(scm, name, SERVICE_QUERY_STATUS) \
        if holder_kind == "self" else None
    watch = DeletedWatch(scm, name)
    say("delete", "ok" if advapi.DeleteService(h) else f"error {err()}")
    ss = SERVICE_STATUS()
    say(f"Q6/{tag} ControlService(STOP) via pre-delete handle",
        "ok" if advapi.ControlService(h, SERVICE_CONTROL_STOP,
                                      ctypes.byref(ss)) else f"error {err()}")
    advapi.CloseServiceHandle(h)
    say(f"Q1/{tag} enumeration while marked", enum_lists(scm, name))
    say(f"Q2/{tag} OpenServiceW while marked", open_result(scm, name))
    say(f"Q3/{tag} GetServiceDisplayNameW while marked",
        display_name(scm, name))
    h2, e2 = create(scm, name, binpath)
    say(f"Q4/{tag} CreateServiceW same name while marked",
        "success" if h2 else f"error {e2}")
    if h2:
        advapi.DeleteService(h2)
        advapi.CloseServiceHandle(h2)
    say(f"Q5/{tag} registry key while marked", key_state(name))
    watch.on_seen = lambda: (key_state(name), enum_lists(scm, name))
    if run:
        # the running service only goes away once its process ends
        subprocess.run(["taskkill", "/F", "/FI", f"SERVICES eq {name}"])
    if child:
        release_holder(child)
    if own:
        advapi.CloseServiceHandle(own)
    ok = watch.wait()
    say(f"Q7/{tag} DELETED notification after last handle closed",
        "delivered" if ok else f"NOT delivered within {FAIL_BOUND_MS} ms")
    if ok:
        say(f"Q8/{tag} at delivery: key / enumeration", watch.at_seen)
    say(f"after/{tag} key / enumeration / open",
        (key_state(name), enum_lists(scm, name), open_result(scm, name)))


def apc_probe():
    ran = []
    cb = APC_CB(lambda _p: ran.append(1))
    ok = kernel.QueueUserAPC(cb, kernel.GetCurrentThread(), None)
    e = err()
    r = kernel.SleepEx(0, True)
    say("Q10 QueueUserAPC(GetCurrentThread())",
        f"queued={bool(ok)} err={e} sleepex={r:#x} ran={bool(ran)}")



SC_MANAGER_CONNECT = 0x1
SC_MANAGER_ENUMERATE_SERVICE = 0x4
SERVICE_NOTIFY_START_PENDING = 0x2
advapi.CloseServiceHandle.restype = w.BOOL


class Recorder:
    """One-shot SERVICE_NOTIFY_2W registrations; records each delivery."""

    def __init__(self, handle, mask):
        self.h = handle
        self.mask = mask
        self.got = []
        self.cb = NOTIFY_CB(self._cb)
        self.n = None
        self.keep = []  # every struct ever registered stays alive

    def arm(self):
        n = SERVICE_NOTIFY_2W()
        n.dwVersion = 2
        n.pfnNotifyCallback = self.cb
        self.keep.append(n)
        rc = advapi.NotifyServiceStatusChangeW(self.h, self.mask,
                                               ctypes.byref(n))
        if rc == 0:
            self.n = n
        return rc

    def _cb(self, _p):
        names = []
        if self.n.pszServiceNames:
            p = self.n.pszServiceNames
            while True:
                s = ctypes.wstring_at(p)
                if not s:
                    break
                names.append(s)
                p += (len(s) + 1) * 2
            kernel.LocalFree(self.n.pszServiceNames)
        self.got.append((self.n.dwNotificationStatus,
                         hex(self.n.dwNotificationTriggered), names,
                         self.n.ServiceStatus.dwCurrentState))

    def immediate(self):
        """Non-blocking: did a callback run right now?"""
        before = len(self.got)
        kernel.SleepEx(0, True)
        return self.got[before:]

    def wait_one(self):
        before = len(self.got)
        start = kernel.GetTickCount64()
        while len(self.got) == before:
            left = FAIL_BOUND_MS - (kernel.GetTickCount64() - start)
            if left <= 0:
                return None
            kernel.SleepEx(int(left), True)
        return self.got[before:]


def make_and_delete(scm, name, binpath):
    h, e = create(scm, name, binpath)
    if not h:
        raise SystemExit(f"create {name}: {e}")
    ok = advapi.DeleteService(h)
    advapi.CloseServiceHandle(h)
    return ok


def rearm_probe(binpath):
    """Does re-arming SERVICE_NOTIFY_DELETED on an SCM handle re-deliver
    names already delivered (a spin), and are deletions made while nothing
    is armed retained?"""
    print("== rearm probe", flush=True)
    full = advapi.OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
    h = advapi.OpenSCManagerW(None, None,
                              SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE)
    r = Recorder(h, SERVICE_NOTIFY_DELETED)
    say("R1 first arm rc", r.arm())
    say("R1 immediate callback after first arm", r.immediate())
    make_and_delete(full, "goetia-probe-ra", binpath)
    say("R2 after deleting A (bounded wait)", r.wait_one())
    rc = r.arm()
    say("R3 re-arm after delivery: rc / immediate", (rc, r.immediate()))
    rc = r.arm()
    say("R3b second arm while one is pending: rc", rc)
    # A registration may now be pending (no immediate callback above).
    make_and_delete(full, "goetia-probe-rb", binpath)
    say("R4 after deleting B with a pending registration", r.wait_one())
    # Nothing armed now: delete C, then arm.
    make_and_delete(full, "goetia-probe-rc", binpath)
    rc = r.arm()
    say("R5 arm after C deleted while unarmed: rc / immediate", (rc, r.immediate()))
    h2 = advapi.OpenSCManagerW(None, None,
                               SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE)
    r2 = Recorder(h2, SERVICE_NOTIFY_DELETED)
    rc = r2.arm()
    say("R6 fresh handle: arm rc / immediate", (rc, r2.immediate()))
    say("R7 all deliveries on H", r.got)


def start_pending_probe():
    """A service whose process never connects to the SCM sits in
    START_PENDING until the SCM's start timeout. What does a STOP get?"""
    import threading
    print("== start-pending probe", flush=True)
    full = advapi.OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
    name = "goetia-probe-sp"
    h, e = create(full, name,
                  r"C:\Windows\System32\cmd.exe /c ping -n 120 127.0.0.1")
    if not h:
        raise SystemExit(f"create {name}: {e}")
    r = Recorder(h, SERVICE_NOTIFY_START_PENDING)
    say("S0 arm rc", r.arm())
    result = {}

    def starter():
        ok = advapi.StartServiceW(h, 0, None)
        result["start"] = "ok" if ok else f"error {ctypes.get_last_error()}"

    t = threading.Thread(target=starter)
    t.start()
    say("S1 START_PENDING notification", r.wait_one())
    st = SERVICE_STATUS_PROCESS()
    need = w.DWORD(0)
    advapi.QueryServiceStatusEx(h, 0, ctypes.byref(st), ctypes.sizeof(st),
                                ctypes.byref(need))
    say("S2 state / controls accepted / pid",
        (st.dwCurrentState, hex(st.dwControlsAccepted), st.dwProcessId))
    ss = SERVICE_STATUS()
    say("S3 ControlService(STOP) while START_PENDING",
        "ok" if advapi.ControlService(h, SERVICE_CONTROL_STOP, ctypes.byref(ss))
        else f"error {err()}")
    out = subprocess.run(["sc.exe", "stop", name], capture_output=True,
                         text=True)
    say("S4 sc stop while START_PENDING",
        (out.returncode, " ".join(out.stdout.split())))
    t.join()
    say("S5 StartServiceW result", result.get("start"))
    if st.dwProcessId:
        subprocess.run(["taskkill", "/F", "/T", "/PID", str(st.dwProcessId)],
                       capture_output=True)
    advapi.DeleteService(h)
    advapi.CloseServiceHandle(h)

def main():
    if len(sys.argv) > 2 and sys.argv[1] == "hold":
        holder(sys.argv[2])
        return
    scm = advapi.OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
    if not scm:
        raise SystemExit(f"OpenSCManagerW: {err()}")
    # Never started, so the binary only has to be a valid path.
    stopped_bin = r"C:\Windows\System32\cmd.exe /c exit 0"
    apc_probe()
    scenario(scm, "child-stopped", stopped_bin, "child", run=False)
    scenario(scm, "self-stopped", stopped_bin, "self", run=False)
    scenario(scm, "none-stopped", stopped_bin, "none", run=False)
    rearm_probe(stopped_bin)
    start_pending_probe()


if __name__ == "__main__":
    main()

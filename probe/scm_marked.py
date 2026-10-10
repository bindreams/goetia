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

# Every notifier object lives for the whole process: the SCM may deliver a
# callback for a registration long after the step that made it (for example
# when its handle closes), and that callback must find live memory.
KEEP_ALIVE = []

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
        self.keep = []
        KEEP_ALIVE.append(self)
        self.arm()

    def arm(self):
        self.n = SERVICE_NOTIFY_2W()
        self.keep.append(self.n)
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
        KEEP_ALIVE.append(self)

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


SERVICE_NOTIFY_STOPPED = 0x1
SERVICE_NOTIFY_RUNNING = 0x8
SERVICE_STOP = 0x20
DELETE_ACCESS = 0x10000


def state_rearm_probe(binpath):
    """Service-handle state notifications on an already-STOPPED service:
    which arms queue a callback immediately?"""
    print("== state re-arm probe", flush=True)
    full = advapi.OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
    name = "goetia-probe-st"
    h, e = create(full, name, binpath)
    if not h:
        raise SystemExit(f"create {name}: {e}")
    r = Recorder(h, SERVICE_NOTIFY_STOPPED)
    say("T1 arm STOPPED on stopped svc: rc / immediate", (r.arm(), r.immediate()))
    say("T2 re-arm STOPPED, same handle: rc / immediate", (r.arm(), r.immediate()))
    r.mask = SERVICE_NOTIFY_RUNNING | SERVICE_NOTIFY_STOPPED
    say("T3 arm RUNNING|STOPPED, same handle: rc / immediate",
        (r.arm(), r.immediate()))
    h2 = advapi.OpenServiceW(full, name, SERVICE_ALL_ACCESS)
    r2 = Recorder(h2, SERVICE_NOTIFY_STOPPED)
    say("T4 fresh handle, arm STOPPED: rc / immediate", (r2.arm(), r2.immediate()))
    advapi.CloseServiceHandle(h2)
    advapi.DeleteService(h)
    advapi.CloseServiceHandle(h)


def access_probe():
    """Does a protected service deny STOP|DELETE to an elevated admin at
    open time, while QUERY_STATUS succeeds? Opens and closes only."""
    print("== access probe", flush=True)
    full = advapi.OpenSCManagerW(None, None, SC_MANAGER_CONNECT)
    for name in ("WinDefend", "wuauserv", "TrustedInstaller"):
        for label, acc in (("QUERY_STATUS", SERVICE_QUERY_STATUS),
                           ("QUERY_STATUS|STOP|DELETE",
                            SERVICE_QUERY_STATUS | SERVICE_STOP | DELETE_ACCESS),
                           ("QUERY_STATUS|STOP", SERVICE_QUERY_STATUS | SERVICE_STOP)):
            h = advapi.OpenServiceW(full, name, acc)
            res = "success" if h else f"error {err()}"
            if h:
                advapi.CloseServiceHandle(h)
            say(f"A {name} {label}", res)
        out = subprocess.run(["sc.exe", "sdshow", name], capture_output=True,
                             text=True)
        say(f"A {name} sdshow", " ".join(out.stdout.split()))


def orphan_key_probe():
    """A Services\\<id> key with no SCM record: what does the SCM answer?"""
    print("== orphan key probe", flush=True)
    name = "goetia-probe-orphan"
    path = rf"SYSTEM\CurrentControlSet\Services\{name}\Parameters"
    k = winreg.CreateKeyEx(winreg.HKEY_LOCAL_MACHINE, path, 0,
                           winreg.KEY_WRITE)
    winreg.SetValueEx(k, "Marker", 0, winreg.REG_SZ, "probe")
    winreg.CloseKey(k)
    full = advapi.OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
    say("O1 OpenServiceW on orphan key", open_result(full, name))
    say("O2 enumeration", enum_lists(full, name))
    out = subprocess.run(["sc.exe", "query", name], capture_output=True,
                         text=True)
    say("O3 sc query", (out.returncode, " ".join(out.stdout.split())))
    winreg.DeleteKey(winreg.HKEY_LOCAL_MACHINE, path)
    winreg.DeleteKey(winreg.HKEY_LOCAL_MACHINE,
                     rf"SYSTEM\CurrentControlSet\Services\{name}")
    say("O4 key after cleanup", key_state(name))


def dacl_probe(binpath):
    """P2: a throwaway service whose DACL denies SD/WP to BA: what does an
    elevated admin get, and can it restore the DACL and delete it?"""
    print("== dacl probe", flush=True)
    full = advapi.OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
    name = "goetia-probe-dacl"
    h, e = create(full, name, binpath)
    if not h:
        raise SystemExit(f"create {name}: {e}")
    advapi.CloseServiceHandle(h)
    orig = subprocess.run(["sc.exe", "sdshow", name], capture_output=True,
                          text=True).stdout.strip()
    say("D0 original sd", orig)
    out = subprocess.run(["sc.exe", "sdset", name,
                          "D:(D;;SDWP;;;BA)(A;;CCLCSWLORCWD;;;BA)"],
                         capture_output=True, text=True)
    say("D1 sdset deny", (out.returncode, " ".join(out.stdout.split())))
    for label, acc in (("QUERY_STATUS", SERVICE_QUERY_STATUS),
                       ("QUERY_STATUS|STOP", SERVICE_QUERY_STATUS | SERVICE_STOP),
                       ("QUERY_STATUS|STOP|DELETE",
                        SERVICE_QUERY_STATUS | SERVICE_STOP | DELETE_ACCESS),
                       ("DELETE", DELETE_ACCESS)):
        h = advapi.OpenServiceW(full, name, acc)
        res = "success" if h else f"error {err()}"
        if h:
            advapi.CloseServiceHandle(h)
        say(f"D2 open {label}", res)
    out = subprocess.run(["sc.exe", "sdset", name, orig], capture_output=True,
                         text=True)
    say("D3 restore sd", (out.returncode, " ".join(out.stdout.split())))
    out = subprocess.run(["sc.exe", "delete", name], capture_output=True,
                         text=True)
    say("D4 sc delete", (out.returncode, " ".join(out.stdout.split())))


def latency_probe(binpath):
    """P1: T probe with a bounded alertable wait after each arm, so 'late'
    and 'never' differ. The bound is the probe's failure bound on an
    external event (the SCM)."""
    print("== latency probe", flush=True)
    full = advapi.OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
    name = "goetia-probe-lat"
    h, e = create(full, name, binpath)
    if not h:
        raise SystemExit(f"create {name}: {e}")

    def bounded(r, ms=2000):
        start = kernel.GetTickCount64()
        before = len(r.got)
        while len(r.got) == before:
            left = ms - (kernel.GetTickCount64() - start)
            if left <= 0:
                return ("none within", ms)
            kernel.SleepEx(int(left), True)
        return (kernel.GetTickCount64() - start, r.got[before:])

    r = Recorder(h, SERVICE_NOTIFY_STOPPED)
    say("L1 arm STOPPED (creating handle)", (r.arm(), bounded(r)))
    say("L2 re-arm STOPPED same handle", (r.arm(), bounded(r)))
    r.mask = SERVICE_NOTIFY_RUNNING | SERVICE_NOTIFY_STOPPED
    say("L3 arm RUNNING|STOPPED same handle", (r.arm(), bounded(r)))
    h2 = advapi.OpenServiceW(full, name, SERVICE_ALL_ACCESS)
    r2 = Recorder(h2, SERVICE_NOTIFY_STOPPED)
    say("L4 fresh handle arm STOPPED", (r2.arm(), bounded(r2)))
    advapi.CloseServiceHandle(h2)
    advapi.DeleteService(h)
    advapi.CloseServiceHandle(h)


SERVICE_START = 0x10


def dacl_rp_and_marked_query_probe(binpath):
    """P2-RP: deny RP/WP to BA: does QUERY_STATUS|START answer 5?
    P3: QueryServiceStatusEx through a handle held across DeleteService,
    on a stopped service that is now marked."""
    print("== dacl RP + marked query probe", flush=True)
    full = advapi.OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
    name = "goetia-probe-rp"
    h, e = create(full, name, binpath)
    if not h:
        raise SystemExit(f"create {name}: {e}")
    advapi.CloseServiceHandle(h)
    orig = subprocess.run(["sc.exe", "sdshow", name], capture_output=True,
                          text=True).stdout.strip()
    out = subprocess.run(["sc.exe", "sdset", name,
                          "D:(D;;RPWP;;;BA)(A;;CCLCSWLOCRRCWDSD;;;BA)"],
                         capture_output=True, text=True)
    say("E1 sdset deny RPWP", (out.returncode, " ".join(out.stdout.split())))
    for label, acc in (("QUERY_STATUS", SERVICE_QUERY_STATUS),
                       ("QUERY_STATUS|START", SERVICE_QUERY_STATUS | SERVICE_START),
                       ("QUERY_STATUS|STOP", SERVICE_QUERY_STATUS | SERVICE_STOP)):
        hh = advapi.OpenServiceW(full, name, acc)
        res = "success" if hh else f"error {err()}"
        if hh:
            advapi.CloseServiceHandle(hh)
        say(f"E2 open {label}", res)
    out = subprocess.run(["sc.exe", "sdset", name, orig], capture_output=True,
                         text=True)
    say("E3 restore sd", (out.returncode, " ".join(out.stdout.split())))
    # P3: hold a handle, delete, query through the held handle.
    held = advapi.OpenServiceW(full, name, SERVICE_ALL_ACCESS)
    say("P3a delete via held handle",
        "ok" if advapi.DeleteService(held) else f"error {err()}")
    st = SERVICE_STATUS_PROCESS()
    need = w.DWORD(0)
    ok = advapi.QueryServiceStatusEx(held, 0, ctypes.byref(st), ctypes.sizeof(st),
                                     ctypes.byref(need))
    say("P3b QueryServiceStatusEx on marked stopped (held handle)",
        f"ok state={st.dwCurrentState}" if ok else f"error {err()}")
    h2 = advapi.OpenServiceW(full, name, SERVICE_QUERY_STATUS)
    if h2:
        ok = advapi.QueryServiceStatusEx(h2, 0, ctypes.byref(st), ctypes.sizeof(st),
                                         ctypes.byref(need))
        say("P3c fresh open + QueryServiceStatusEx while marked",
            f"ok state={st.dwCurrentState}" if ok else f"error {err()}")
        advapi.CloseServiceHandle(h2)
    else:
        say("P3c fresh open while marked", f"error {err()}")
    advapi.CloseServiceHandle(held)
    say("P3d key after close", key_state(name))


REG_NOTIFY_CHANGE_NAME = 0x1
advapi.RegNotifyChangeKeyValue.restype = ctypes.c_long
advapi.RegNotifyChangeKeyValue.argtypes = [ctypes.c_void_p, w.BOOL, w.DWORD,
                                           ctypes.c_void_p, w.BOOL]
kernel.CreateEventW.restype = ctypes.c_void_p
kernel.CreateEventW.argtypes = [ctypes.c_void_p, w.BOOL, w.BOOL, w.LPCWSTR]
kernel.WaitForSingleObject.restype = w.DWORD
kernel.WaitForSingleObject.argtypes = [ctypes.c_void_p, w.DWORD]


def watch(key, subtree=False):
    ev = kernel.CreateEventW(None, False, False, None)
    rc = advapi.RegNotifyChangeKeyValue(key.handle, subtree,
                                        REG_NOTIFY_CHANGE_NAME, ev, True)
    return ev, rc


def waited(ev, ms=3000):
    r = kernel.WaitForSingleObject(ev, ms)
    return {0: "signalled", 0x102: f"not signalled within {ms} ms"}.get(r, hex(r))


def registry_probe(binpath):
    """Does REG_NOTIFY_CHANGE_NAME on a key signal when that key itself is
    deleted, versus watching its parent? (Bounded waits: probe failure bound
    on an external event.)"""
    print("== registry probe", flush=True)
    HK = winreg.HKEY_LOCAL_MACHINE
    base = r"SOFTWARE\goetia-probe"
    winreg.CreateKey(HK, base + r"\k")
    k = winreg.OpenKey(HK, base + r"\k", 0, winreg.KEY_NOTIFY)
    ev, rc = watch(k)
    winreg.DeleteKey(HK, base + r"\k")
    say("G1 watch key itself, delete it", (rc, waited(ev)))
    winreg.CloseKey(k)
    winreg.CreateKey(HK, base + r"\k")
    parent = winreg.OpenKey(HK, base, 0, winreg.KEY_NOTIFY)
    ev, rc = watch(parent)
    winreg.DeleteKey(HK, base + r"\k")
    say("G2 watch parent, delete child", (rc, waited(ev)))
    winreg.CloseKey(parent)
    winreg.DeleteKey(HK, base)
    # SCM deletion of a service key with a Parameters subkey.
    full = advapi.OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
    name = "goetia-probe-reg"
    h, e = create(full, name, binpath)
    if not h:
        raise SystemExit(f"create {name}: {e}")
    svc = rf"SYSTEM\CurrentControlSet\Services\{name}"
    pk = winreg.CreateKeyEx(HK, svc + r"\Parameters", 0, winreg.KEY_WRITE)
    winreg.SetValueEx(pk, "Marker", 0, winreg.REG_SZ, "probe")
    winreg.CloseKey(pk)
    own = winreg.OpenKey(HK, svc, 0, winreg.KEY_NOTIFY)
    ev_own, rc_own = watch(own)
    services = winreg.OpenKey(HK, r"SYSTEM\CurrentControlSet\Services", 0,
                              winreg.KEY_NOTIFY)
    ev_par, rc_par = watch(services)
    r = Recorder(full, SERVICE_NOTIFY_DELETED)
    r.arm()
    advapi.DeleteService(h)
    advapi.CloseServiceHandle(h)
    got = r.wait_one()
    try:
        kk = winreg.OpenKey(HK, svc)
        subs = []
        i = 0
        while True:
            try:
                subs.append(winreg.EnumKey(kk, i))
            except OSError:
                break
            i += 1
        winreg.CloseKey(kk)
        at = f"present, subkeys={subs}"
    except FileNotFoundError:
        at = "absent"
    say("G3 at DELETED delivery: service key", (got is not None, at))
    say("G4 watch on Services\\<id> itself during SCM delete", (rc_own, waited(ev_own)))
    say("G5 watch on Services parent during SCM delete", (rc_par, waited(ev_par, 1)))
    say("G6 key after", key_state(name))


def key_contents(path):
    HK = winreg.HKEY_LOCAL_MACHINE
    try:
        k = winreg.OpenKey(HK, path)
    except FileNotFoundError:
        return "absent"
    subs, vals = [], []
    i = 0
    while True:
        try:
            subs.append(winreg.EnumKey(k, i))
        except OSError:
            break
        i += 1
    i = 0
    while True:
        try:
            vals.append(winreg.EnumValue(k, i)[0])
        except OSError:
            break
        i += 1
    winreg.CloseKey(k)
    return f"subkeys={subs} values={vals}"


def lingering_key_probe(binpath):
    """P7: right after an unheld delete, while OpenServiceW already says
    1060, what does Services\\<id> still hold? Sampled 5 times."""
    print("== lingering key probe", flush=True)
    HK = winreg.HKEY_LOCAL_MACHINE
    full = advapi.OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
    for n in range(5):
        name = f"goetia-probe-lk{n}"
        h, e = create(full, name, binpath)
        if not h:
            raise SystemExit(f"create {name}: {e}")
        svc = rf"SYSTEM\CurrentControlSet\Services\{name}"
        pk = winreg.CreateKeyEx(HK, svc + r"\Parameters", 0, winreg.KEY_WRITE)
        winreg.SetValueEx(pk, "Marker", 0, winreg.REG_SZ, "probe")
        winreg.CloseKey(pk)
        advapi.DeleteService(h)
        advapi.CloseServiceHandle(h)
        o = open_result(full, name)
        say(f"P7.{n} open / key / Parameters",
            (o, key_contents(svc), key_contents(svc + r"\Parameters")))


def deleted_key_read_probe():
    """M-4: read values through a handle opened before the key was deleted."""
    print("== deleted-key read probe", flush=True)
    HK = winreg.HKEY_LOCAL_MACHINE
    base = r"SOFTWARE\goetia-probe2"
    k = winreg.CreateKeyEx(HK, base + r"\k", 0, winreg.KEY_WRITE)
    winreg.SetValueEx(k, "v", 0, winreg.REG_SZ, "x")
    winreg.CloseKey(k)
    h1 = winreg.OpenKey(HK, base + r"\k", 0, winreg.KEY_READ)
    advapi.RegDeleteTreeW.argtypes = [ctypes.c_void_p, w.LPCWSTR]
    advapi.RegDeleteTreeW.restype = ctypes.c_long
    rc = advapi.RegDeleteTreeW(int(HK), base)
    say("K1 RegDeleteTreeW rc", rc)
    for label, fn in (("RegQueryValueEx(v)", lambda: winreg.QueryValueEx(h1, "v")),
                      ("RegEnumValue(0)", lambda: winreg.EnumValue(h1, 0))):
        try:
            say(f"K2 {label} on pre-delete handle", f"ok {fn()!r}")
        except OSError as ex:
            say(f"K2 {label} on pre-delete handle",
                f"error winerror={getattr(ex, 'winerror', None)} {ex}")
    winreg.CloseKey(h1)


def held_lingering_probe(binpath):
    """P7b: delete while a CHILD holds a handle; close it; at the first 1060
    list Services\\<id> and Parameters once; then wait for the key to vanish
    via the Services parent watch (bounded: probe failure bound on an
    external event) and print elapsed ms."""
    print("== held lingering probe", flush=True)
    HK = winreg.HKEY_LOCAL_MACHINE
    full = advapi.OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
    for n in range(5):
        name = f"goetia-probe-hl{n}"
        h, e = create(full, name, binpath)
        if not h:
            raise SystemExit(f"create {name}: {e}")
        svc = rf"SYSTEM\CurrentControlSet\Services\{name}"
        pk = winreg.CreateKeyEx(HK, svc + r"\Parameters", 0, winreg.KEY_WRITE)
        winreg.SetValueEx(pk, "Marker", 0, winreg.REG_SZ, "probe")
        winreg.CloseKey(pk)
        child = spawn_holder(name)
        advapi.DeleteService(h)
        advapi.CloseServiceHandle(h)
        r = Recorder(full, SERVICE_NOTIFY_DELETED)
        r.arm()
        release_holder(child)
        got = r.wait_one()
        o = open_result(full, name)
        snap = (o, key_contents(svc), key_contents(svc + r"\Parameters"))
        services = winreg.OpenKey(HK, r"SYSTEM\CurrentControlSet\Services", 0,
                                  winreg.KEY_NOTIFY)
        start = kernel.GetTickCount64()
        ev, rc = watch(services)
        while key_state(name) != "absent":
            left = 10000 - (kernel.GetTickCount64() - start)
            if left <= 0:
                break
            kernel.WaitForSingleObject(ev, int(left))
            ev, rc = watch(services)
        el = kernel.GetTickCount64() - start
        say(f"P7b.{n} delivered / at-first-check / key-after / ms",
            (got is not None, snap, key_state(name), el))
        winreg.CloseKey(services)


def inprocess_lingering_probe(binpath):
    """P7c: T5's path. Open a second in-process handle, DeleteService via the
    first, close both; at the first 1060 list Services\\<id> and Parameters;
    then wait for absence via the Services parent watch (bounded failure
    bound on an external event) and print elapsed ms. 5 samples."""
    print("== in-process lingering probe", flush=True)
    HK = winreg.HKEY_LOCAL_MACHINE
    full = advapi.OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
    for n in range(5):
        name = f"goetia-probe-ip{n}"
        h, e = create(full, name, binpath)
        if not h:
            raise SystemExit(f"create {name}: {e}")
        svc = rf"SYSTEM\CurrentControlSet\Services\{name}"
        pk = winreg.CreateKeyEx(HK, svc + r"\Parameters", 0, winreg.KEY_WRITE)
        winreg.SetValueEx(pk, "Marker", 0, winreg.REG_SZ, "probe")
        winreg.CloseKey(pk)
        h2 = advapi.OpenServiceW(full, name, SERVICE_QUERY_STATUS)
        advapi.DeleteService(h)
        h3 = advapi.OpenServiceW(full, name, SERVICE_QUERY_STATUS)  # opened while marked
        advapi.CloseServiceHandle(h)
        advapi.CloseServiceHandle(h2)
        if h3:
            advapi.CloseServiceHandle(h3)
        o = open_result(full, name)
        snap = (o, key_contents(svc), key_contents(svc + r"\Parameters"))
        services = winreg.OpenKey(HK, r"SYSTEM\CurrentControlSet\Services", 0,
                                  winreg.KEY_NOTIFY)
        start = kernel.GetTickCount64()
        ev, rc = watch(services)
        while key_state(name) != "absent":
            left = 10000 - (kernel.GetTickCount64() - start)
            if left <= 0:
                break
            kernel.WaitForSingleObject(ev, int(left))
            ev, rc = watch(services)
        el = kernel.GetTickCount64() - start
        say(f"P7c.{n} at-first-check / key-after / ms", (snap, key_state(name), el))
        winreg.CloseKey(services)


def hkcu_deleted_key_read_probe():
    """K-HKCU: read values through a handle whose key was deleted, under HKCU."""
    print("== HKCU deleted-key read probe", flush=True)
    HK = winreg.HKEY_CURRENT_USER
    base = r"Software\goetia-probe3"
    k = winreg.CreateKeyEx(HK, base + r"\k\Parameters", 0, winreg.KEY_WRITE)
    winreg.SetValueEx(k, "Marker", 0, winreg.REG_SZ, "x")
    winreg.CloseKey(k)
    h1 = winreg.OpenKey(HK, base + r"\k\Parameters", 0, winreg.KEY_READ)
    say("KC0 read before delete", winreg.QueryValueEx(h1, "Marker"))
    advapi.RegDeleteTreeW.argtypes = [ctypes.c_void_p, w.LPCWSTR]
    advapi.RegDeleteTreeW.restype = ctypes.c_long
    say("KC1 RegDeleteTreeW rc", advapi.RegDeleteTreeW(int(HK), base))
    for label, fn in (("RegQueryValueEx", lambda: winreg.QueryValueEx(h1, "Marker")),
                      ("RegEnumValue(0)", lambda: winreg.EnumValue(h1, 0))):
        try:
            say(f"KC2 {label} on pre-delete handle", f"ok {fn()!r}")
        except OSError as ex:
            say(f"KC2 {label} on pre-delete handle", f"error winerror={getattr(ex, 'winerror', None)}")
    winreg.CloseKey(h1)


def scm_delete_open_parameters_probe(binpath):
    """KS: hold a KEY_READ handle on Services\\<id>\\Parameters, let the SCM
    delete the service (no other handle), then read through the held handle."""
    print("== SCM-delete under open Parameters handle probe", flush=True)
    HK = winreg.HKEY_LOCAL_MACHINE
    full = advapi.OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
    for n in range(3):
        name = f"goetia-probe-ks{n}"
        h, e = create(full, name, binpath)
        if not h:
            raise SystemExit(f"create {name}: {e}")
        svc = rf"SYSTEM\CurrentControlSet\Services\{name}"
        pk = winreg.CreateKeyEx(HK, svc + r"\Parameters", 0, winreg.KEY_WRITE)
        winreg.SetValueEx(pk, "Marker", 0, winreg.REG_SZ, "probe")
        winreg.CloseKey(pk)
        held = winreg.OpenKey(HK, svc + r"\Parameters", 0, winreg.KEY_READ)
        r = Recorder(full, SERVICE_NOTIFY_DELETED)
        r.arm()
        advapi.DeleteService(h)
        advapi.CloseServiceHandle(h)
        got = r.wait_one()
        res = []
        for label, fn in (("Query", lambda: winreg.QueryValueEx(held, "Marker")),
                          ("Enum0", lambda: winreg.EnumValue(held, 0))):
            try:
                res.append((label, "ok", fn()))
            except OSError as ex:
                res.append((label, "error", getattr(ex, "winerror", None)))
        winreg.CloseKey(held)
        say(f"KS.{n} delivered / reads on held Parameters / key after",
            (got is not None, res, key_state(name)))

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
    state_rearm_probe(stopped_bin)
    access_probe()
    orphan_key_probe()
    dacl_probe(stopped_bin)
    latency_probe(stopped_bin)
    dacl_rp_and_marked_query_probe(stopped_bin)
    registry_probe(stopped_bin)
    lingering_key_probe(stopped_bin)
    deleted_key_read_probe()
    held_lingering_probe(stopped_bin)
    hkcu_deleted_key_read_probe()
    scm_delete_open_parameters_probe(stopped_bin)
    inprocess_lingering_probe(stopped_bin)


if __name__ == "__main__":
    main()

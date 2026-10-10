#!/usr/bin/env python3
"""Step 0 report. Throwaway; stdlib only.

  aggregate.py --check DIR      every DIR/<job>/*.json parses and has id, os and verdict (exit 1 if not)
  aggregate.py report DIR       one-OS report from DIR/<job>/*.json -> DIR/report.md (+ $GITHUB_STEP_SUMMARY)
  aggregate.py combine ROOT OUT every ROOT/*/<job>/*.json from all artifacts, OSes side by side
                                -> OUT/report.md (+ $GITHUB_STEP_SUMMARY)
"""
import glob
import json
import os
import sys
from collections import defaultdict

MIB = 1 << 20


def load(pattern):
    recs, bad = [], []
    for p in sorted(glob.glob(pattern)):
        if p.endswith("report.md") or not p.endswith(".json"):
            continue
        try:
            with open(p) as f:
                r = json.load(f)
            if not isinstance(r, dict):
                raise ValueError("not an object")
            missing = [k for k in ("id", "os", "verdict") if k not in r]
            if missing:
                raise ValueError(f"missing {missing}")
            r["_path"] = p
            recs.append(r)
        except Exception as e:  # noqa: BLE001 - any failure is "malformed"
            bad.append(f"{p}: {e}")
    return recs, bad


def os_key(r):
    o = r.get("os") or {}
    return o.get("image", "?")


def cell(r):
    """A short observed value for one record."""
    if r is None:
        return "-"
    o = r.get("observed") or {}
    i = r["id"]
    v = r["verdict"]
    if v == "blocked":
        pf = o.get("phase_file")
        last = "?"
        try:
            with open(os.path.join(os.path.dirname(r["_path"]), pf)) as f:
                last = f.read().strip().splitlines()[-1]
        except Exception:  # noqa: BLE001
            pass
        return "BLOCKED (no verdict before the step timeout); last phase: " + last
    if i.startswith("P2."):
        return cell_p2(r, i, v, o)
    if i == "B1":
        return "nlink " + ",".join(str(x.get("nlink")) for x in o.get("cumulative", []))
    if i == "B2":
        return "fsid vardir=%s ld=%s" % (o.get("vardir", {}).get("fsid"), o.get("library_launchdaemons", {}).get("fsid"))
    if i == "B3":
        return "%s; path=%s; btm changed=%s" % (v, o.get("print_path_line"), o.get("btm_changed"))
    if i.startswith("B4."):
        return "accepted" if o.get("accepted") else "refused: " + str((o.get("bootstrap") or {}).get("stderr", "")).strip()[:60]
    if i == "B5":
        def n(k, needle):
            sn = o.get(k) or {}
            return (sn.get("mentions") or {}).get(needle)
        return "loaded=%s btm uuid lines %s/%s/%s/%s; changed(bootstrap->rewrite)=%s; rebootstrap=%s" % (
            o.get("still_loaded"), *[(o.get(k) or {}).get("uuid_lines") for k in ("btm_0_before_bootstrap", "btm_1_after_bootstrap", "btm_2_after_rewrite", "btm_3_end")],
            o.get("btm_changed_bootstrap_to_rewrite"), o.get("bootstrap_again_accepted"))
    if i == "B6":
        return "limit=%s" % (o.get("limit") if o.get("limit") is not None else o.get("note"))
    if i.startswith("D"):
        return v + (" held=%s" % o.get("held_fds") if v == "ran" and o.get("held_fds") is not None else "")
    if i.startswith("T1.") or i.startswith("N4."):
        return v
    if i == "T2":
        return "%s (%s matching lines)" % (v, o.get("matched"))
    if i == "N1":
        return "MNT_LOCAL=%s pathconf=%s/%s" % (o.get("mnt_local"), (o.get("pathconf_vers3") or {}).get("value"), (o.get("pathconf_noopaque_auth") or {}).get("value"))
    if i == "N2":
        return "root errno=%s; account=%s" % (o.get("root_fstatat_private_inner"), (o.get("account_search_open_then_fstatat") or {}).get("values"))
    if i == "N3":
        return "denied=%s allowed=%s" % (((o.get("acl_denied") or {}).get("values") or {}).get("errno"), ((o.get("allowed") or {}).get("values") or {}).get("errno"))
    if i == "N5":
        sm = ((o.get("forms_tried") or [{}])[0].get("showmount") or {})
        return "polls=0 offline=%s; first showmount code=%s %s" % (o.get("offline_seen_first"), sm.get("code"), (sm.get("stderr") or sm.get("stdout") or "").strip()[:60])
    if i == "M1":
        return "; ".join("%s=%s" % (a["attempt"][:1], "ok" if a["succeeded"] else ((a["result"].get("stderr") or "").strip()[:40] or "fail")) for a in o.get("attempts", []))
    if i == "M2":
        return "; ".join(m.get("mntonname", "?") for m in o.get("mounts_listed", [])) or "none"
    if i == "M3":
        return "%s; returned=%s listed=%s stalled=%s" % (v, o.get("getfsstat_returned"), o.get("mount_listed"), o.get("stall_reproduced"))
    if i == "Q2":
        return "shared=%s (n1=%s l1=%s l2=%s)" % (o.get("shared_description"), o.get("n1_bytes"), o.get("lseek_fd1"), o.get("lseek_fd2"))
    if i.startswith("Q1"):
        return "first=%s second=%s" % (o.get("first"), (o.get("second") or {}).get("verdict"))
    return v


# Pass 2 (A1 round 10) ------------------------------------------------------------------------------

EAGAIN = "-1,35"  # return value and errno of a write that failed with EAGAIN on macOS
ESRCH = 3


def p2_launches(r):
    """Every launch a (possibly blocked) M2 record holds."""
    o = r.get("observed") or {}
    if r["verdict"] == "blocked":
        return (o.get("partial") or {}).get("launches_done") or []
    return o.get("launches") or []


def m2_stats(launches):
    st = dict(launches=len(launches), receipts_ok=0, esrch=0, exited78=0, ran=0, no_pid=0, other_exit=[], not_attached=0)
    for l in launches:
        rec = l.get("record") or {}
        v = l.get("verdict")
        if rec.get("receipt_proc") == 0:
            st["receipts_ok"] += 1
        elif rec.get("receipt_proc") == ESRCH:
            st["esrch"] += 1
        elif rec.get("receipt_proc") is not None:
            st["not_attached"] += 1
        if rec.get("exit") == "exited:78":
            st["exited78"] += 1
        if v == "ran":
            st["ran"] += 1
        elif v == "no-pid":
            st["no_pid"] += 1
        elif v == "exit-other":
            st["other_exit"].append(rec.get("exit"))
    return st


def cell_p2(r, i, v, o):
    if v == "blocked":
        pf, last = o.get("phase_file"), "?"
        try:
            with open(os.path.join(os.path.dirname(r["_path"]), pf)) as f:
                last = f.read().strip().splitlines()[-1]
        except Exception:  # noqa: BLE001
            pass
        extra = ""
        if i.startswith("P2.M2"):
            extra = "; launches done: %d" % len(p2_launches(r))
        return "BLOCKED (no verdict before the step timeout)%s; last phase: %s" % (extra, last)
    if i.startswith("P2.M1."):
        return "%s w1=%s w2=%s nonblock=%s" % (v, o.get("w1"), o.get("w2"), o.get("nonblock_set"))
    if i == "P2.M2b":
        return "%d sequences" % len(o.get("sequences") or [])
    if i.startswith("P2.M2."):
        s = m2_stats(p2_launches(r))
        return "%d launches: receipts ok %d, ESRCH %d, exited:78 %d, ran %d%s" % (
            s["launches"], s["receipts_ok"], s["esrch"], s["exited78"], s["ran"],
            ", other exits %s" % s["other_exit"] if s["other_exit"] else "")
    if i.startswith("P2.M3."):
        if o.get("mounted"):
            return "mounted (%.2fs)" % o.get("elapsed_seconds", -1)
        m = o.get("mount") or {}
        return "NOT mounted: code %s %s (%.2fs)" % (m.get("code"), (m.get("stderr") or "").strip()[:80], o.get("elapsed_seconds", -1))
    if i.startswith("P2.M4."):
        return "sandbox_init=%s; denied: %s" % ((o.get("sandbox_init") or {}).get("ret"), o.get("denied_steps"))
    return v


def p2_get(by, rid, o):
    return by.get((rid, o))


def p2_m1(by, oses):
    out = ["## Pass 2 M1 ptmx-nonblock: rule branch"]
    blocked, bad, ctl = [], [], []
    rows = []
    for o in oses:
        for which in ("control", "nobody", "root"):
            r = p2_get(by, "P2.M1." + which, o)
            ob = (r or {}).get("observed") or {}
            ran = r is not None and r["verdict"] == "ran"
            if which == "control":
                ok = ran and all(str(ob.get(k, "")).split(",")[0].lstrip("-").isdigit() and not str(ob.get(k)).startswith("-") and str(ob.get(k)).endswith(",0") for k in ("w1", "w2")) and ob.get("nonblock_set") is True
                if not ok:
                    ctl.append(o)
            elif not ran:
                blocked.append(f"{o}/{which}")
            elif not (ob.get("w1") == EAGAIN and ob.get("w2") == EAGAIN):
                bad.append(f"{o}/{which}: w1={ob.get('w1')} w2={ob.get('w2')}")
            rows.append([o, which, ob.get("w1", "-"), ob.get("w2", "-"), ob.get("getfl_before", "-"), ob.get("getfl_after", "-"),
                         r["verdict"] if r else "missing"])
    out += table(rows, ["os", "row", "write 1 (ret,errno)", "write 2 (ret,errno)", "F_GETFL before", "F_GETFL after", "verdict"])
    out.append("")
    out.append("EAGAIN is `-1,35`. The control is `/dev/null` with the flag: both writes must return a positive count with errno 0.")
    if ctl:
        out.append(f"- **CONTROL FAILED** on {ctl}: M1 is untrusted.")
    if blocked:
        out.append(f"- **Branch 3**: the sentinel never reached its ready line (blocked or missing) on {blocked}: the stall is not the first write. "
                   "Same plan change as branch 2 (Q1b (iv) paragraph and D19/D20 reworded to \"stalls for a reason not established\"); the deputy decides whether to sample the stuck process.")
    if bad:
        out.append(f"- **Branch 2**: a write did not return EAGAIN: {bad}. Plan change: reword Q1b (iv) and D19/D20 to \"stalls for a reason not established\"; sheet 9.1 follows; the deputy reviews.")
    if not blocked and not bad and not ctl:
        out.append("- **Branch 1**: both writes returned -1/EAGAIN as both users on both OSes: the kernel reading is confirmed; no plan change.")
    return out + [""]


def p2_m2(by, oses):
    out = ["## Pass 2 M2 suspended-start: rule branch"]
    fixtures = ("cwd", "log", "gap", "control")
    rows, rules = [], {}
    for o in oses:
        trig = set()
        clean = True
        for fx in fixtures:
            r = p2_get(by, "P2.M2." + fx, o)
            if r is None:
                rows.append([o, fx, "missing"] + ["-"] * 6)
                clean = False
                if fx == "control":
                    trig.add(5)
                continue
            launches = p2_launches(r)
            s = m2_stats(launches)
            blocked = r["verdict"] == "blocked"
            want = 10 if fx == "control" else 33
            rows.append([o, fx, "BLOCKED" if blocked else "done", s["launches"], s["receipts_ok"], s["esrch"], s["exited78"],
                         s["ran"], s["other_exit"] or "-"])
            if s["no_pid"] or any(l.get("verdict") == "bootstrap-refused" for l in launches):
                trig.add(4)
            if fx == "control":
                if blocked or s["ran"] != want or s["launches"] != want:
                    trig.add(5)
            else:
                if blocked:
                    clean = False
                if s["esrch"]:
                    trig.add(2)
                if s["other_exit"]:
                    trig.add(3)
                if s["ran"] or s["launches"] != want or s["exited78"] != s["launches"] or s["receipts_ok"] != s["launches"]:
                    clean = False
            if fx == "control" and (s["receipts_ok"] != s["launches"]):
                clean = False
        rules[o] = trig or ({1} if clean else set())
    out += table(rows, ["os", "fixture", "state", "launches", "receipts OK", "ESRCH receipts", "exited:78", "Ran", "other exit values"])
    out.append("")
    for o in oses:
        t = sorted(rules[o])
        out.append(f"- {o}: " + (("rule " + ", ".join(map(str, t))) if t else "no rule matches mechanically (a refused fixture ran, or counts are short): read the table"))
    allr = set().union(*rules.values()) - {1} if rules else set()
    if not allr and rules and all(rules[o] == {1} for o in oses):
        out.append("- **Overall: rule 1**: adopt A1 Task 1 \"Starting and watching the job\" as written in rev 21; `ESRCH` stays an anomaly.")
    else:
        out.append("- **Overall: rules " + (", ".join(map(str, sorted(allr))) or "none") + "** (rule 1 holds only where listed above as such): " + "; ".join(
            {2: "2: a refused launch had an ESRCH receipt: -s does not remove the race; deputy chooses (a) ESRCH+drained STATUS+CTRL held, exit unknown, or (b) keep the exit-code requirement",
             3: "3: receipts OK but a refused launch's exit is not 78: reject the sequence",
             4: "4: kickstart -s refused or printed no pid: same as 3",
             5: "5: the Ran control did not run after SIGCONT: needs a different resume; report ps and print data",
             1: "1"}[x] for x in sorted(allr)))
    out.append("")
    # M2b
    out.append("### M2b stale-exit-code")
    rows, total = [], {}
    for o in oses:
        r = p2_get(by, "P2.M2b", o)
        if r is None:
            rows.append([o, "missing"])
            continue
        seqs = (r.get("observed") or {}).get("sequences") or (r.get("observed", {}).get("partial") or {}).get("sequences_done") or []
        live = stale = 0
        per = {}
        for sq in seqs:
            for l in sq.get("launches", []):
                n = l.get("i")
                rec = l.get("record") or {}
                for key in ("print_suspended", "print_ready"):
                    pr = rec.get(key)
                    if pr and pr.get("pid_line"):
                        live += 1
                        code = str(pr.get("last_exit_code") or "")
                        is78 = code.startswith("78")
                        stale += is78
                        d = per.setdefault((n, key), [0, 0])
                        d[0] += 1
                        d[1] += is78
        total[o] = (live, stale)
        rows.append([o, len(seqs), live, stale, "; ".join(f"launch {n} {k}: {b}/{a}" for (n, k), (a, b) in sorted(per.items()))])
    out += table(rows, ["os", "sequences", "live-pid prints", "of them last exit code 78", "by launch and print (78/live)"])
    out.append("")
    if any(st for _, st in total.values()):
        out.append("- **Rule 6, stale 78 seen**: A1's sentence \"a `print` is never consulted for the verdict, because a stale 78 cannot be told from a new one\" is confirmed as measured; cite this row.")
    else:
        out.append("- **Rule 6, no stale 78 in any live-pid print**: the sentence stays (the verdict never reads `print`), but \"cannot be told\" is reworded to \"might not be told\". Neither outcome changes verdict logic.")
    return out + [""]


def p2_m3(by, oses):
    out = ["## Pass 2 M3 nfs-shipped: rule branch"]
    forms = ("showmount", "default", "retrycnt0")
    res, rows = {}, []
    for o in oses:
        for f in forms:
            recs = [by[(f"P2.M3.{f}.{n}", o)] for n in range(1, 6) if (f"P2.M3.{f}.{n}", o) in by]
            mounted = sum(1 for r in recs if (r.get("observed") or {}).get("mounted") is True)
            res[(o, f)] = (mounted, len(recs))
            rows.append([o, f, f"{mounted} of {len(recs)}" + ("" if len(recs) == 5 else " (INCOMPLETE: want 5 results)")])
    out += table(rows, ["os", "form", "mounted first time"])
    out.append("")
    for o in oses:
        sm, de, rc = (res[(o, f)] for f in forms)
        full = lambda x: x == (5, 5)  # noqa: E731
        if full(de):
            out.append(f"- {o}: `default` 5 of 5: **A1's setup step 5 stands as written** (no showmount, no wait). Evidence line: \"{sm[0]} of {sm[1]} with a preceding showmount, 5 of 5 without\". No `retrycnt` option. The README and plan keep \"the leg can flake red; a human re-runs it\".")
        else:
            iv = " Option (iv) (a single showmount as a warm-up, exit status ignored) is supported by the data." if full(sm) else ""
            out.append(f"- {o}: `default` {de[0]} of {de[1]}: the no-wait ruling is contradicted for the shipped sequence. No plan change by itself; the failure data goes to the deputy, who chooses among (i) `-o retrycnt=<k>`, (ii) a wait on an external event the data identifies, (iii) accept and document the flake rate, (iv) showmount warm-up.{iv}")
        if not full(sm):
            out.append(f"- {o}: `showmount` {sm[0]} of {sm[1]}: it passed 10 of 10 in pass 1, so that evidence was luck; treat as the `default` failure row.")
        if full(rc):
            out.append(f"- {o}: `retrycnt0` 5 of 5: the first attempt wins without mount_nfs's retry; A1 records that.")
        elif full(de):
            out.append(f"- {o}: `retrycnt0` {rc[0]} of {rc[1]} where `default` passes: the leg depends on mount_nfs's documented second attempt; A1 says so in step 5 (the quick 8 s timeout confounds this).")
        else:
            out.append(f"- {o}: `retrycnt0` {rc[0]} of {rc[1]} (`default` also failed).")
    return out + [""]


def p2_m4(by, oses):
    out = ["## Pass 2 M4 sandbox-steps: rule branch"]
    cols = [(o, u) for o in oses for u in ("root", "nobody") if (f"P2.M4.{u}", o) in by]
    if not cols:
        return out + ["no M4 results", ""]
    steps = []
    for c in cols:
        for st in (by[(f"P2.M4.{c[1]}", c[0])].get("observed") or {}).get("steps") or []:
            if st["target"] == "D" and st["step"] not in steps:
                steps.append(st["step"])
    def d_result(c, name):
        for st in (by[(f"P2.M4.{c[1]}", c[0])].get("observed") or {}).get("steps") or []:
            if st["target"] == "D" and st["step"] == name:
                return st["result"]
        return "-"
    rows = [["sandbox_init return"] + [str((by[(f"P2.M4.{u}", o)].get("observed") or {}).get("sandbox_init", {}).get("ret")) for o, u in cols]]
    rows.append(["sandbox_init errbuf"] + [str((by[(f"P2.M4.{u}", o)].get("observed") or {}).get("sandbox_init", {}).get("errbuf")) for o, u in cols])
    rows += [[s] + [str(d_result(c, s)) for c in cols] for s in steps]
    out += table(rows, ["step on D (errno, 1 = EPERM)"] + [f"{o} / {u}" for o, u in cols])
    out.append("")
    untrusted = [c for c in cols if any("control C" in a for a in by[(f"P2.M4.{c[1]}", c[0])].get("anomalies") or [])]
    if untrusted:
        out.append(f"- **CONTROL FAILED** (C not clean) for {untrusted}: untrusted.")
    failing = sorted({o for o, u in cols if (by[(f"P2.M4.{u}", o)].get("observed") or {}).get("sandbox_init", {}).get("ret") != 0})
    if failing:
        out.append(f"- **sandbox_init fails** on {failing}: the Q9 (b) tests cannot run there and must not skip; the deputy picks another denial source for that OS, or the plan states which OS the tests cover.")
    nosearch = sorted({o for o, u in cols if "open(T, O_SEARCH)" not in ((by[(f"P2.M4.{u}", o)].get("observed") or {}).get("denied_steps") or [])})
    if nosearch:
        out.append(f"- **The O_SEARCH open is not denied** on {nosearch}: `goetias_own_eperm_proceeds_with_one_notice` has no denial to observe there; same action as the previous bullet.")
    if not failing:
        for u in ("root", "nobody"):
            per_os = {}
            for o in oses:
                if (f"P2.M4.{u}", o) in by:
                    per_os[o] = {s for s in steps if d_result((o, u), s) == 1}
            if not per_os:
                continue
            both = sorted(set.intersection(*per_os.values()))
            some = sorted(set.union(*per_os.values()) - set(both))
            out.append(f"- {u}: denied with EPERM on every OS (test variants that deny at the step): {both or 'none'}; denied on some OSes only (per-OS expectations): {some or 'none'}; every other step is \"not covered by this fixture\".")
    return out + [""]


def pass2(recs, oses):
    by = {(r["id"], os_key(r)): r for r in recs}
    if not any(r["id"].startswith("P2.") for r in recs):
        return []
    out = ["## Pass 2 (A1 round 10): rule branches", ""]
    return out + p2_m1(by, oses) + p2_m2(by, oses) + p2_m3(by, oses) + p2_m4(by, oses)


def by_question(recs):
    q = defaultdict(lambda: defaultdict(dict))
    for r in recs:
        if r["id"] == "RUNNER":
            continue
        for name in r.get("question") or ["(harness self-checks)"]:
            q[name][r["id"]][os_key(r)] = r
    return q


def fmt(x):
    return str(x).replace("|", "\\|").replace("\n", " ")


def table(rows, header):
    out = ["| " + " | ".join(header) + " |", "|" + "---|" * len(header)]
    out += ["| " + " | ".join(fmt(c) for c in r) + " |" for r in rows]
    return out


CONTROLS = [
    ("D01 /dev/null is Ran", "D01", lambda r: r["verdict"] == "ran"),
    ("B4.p0 root:wheel 0644 accepted", "B4.p0", lambda r: r["observed"].get("accepted") is True),
    ("B4.p4 o+w refused", "B4.p4", lambda r: r["observed"].get("accepted") is False),
    ("B1 empty directory has st_nlink 2", "B1", lambda r: (r["observed"].get("cumulative") or [{}])[0].get("nlink") == 2),
    ("B2 fsid(/private/var/db) == fsid(/Library/LaunchDaemons)", "B2", lambda r: not any("control" in a for a in r["anomalies"])),
    ("B3 sentinel from /Library/LaunchDaemons is Ran", "B3", lambda r: r["observed"].get("control") == "ran"),
    ("Q1c deny add_file is Refused", "Q1c", lambda r: r["verdict"] == "refused"),
    ("Q2 clean launch is Ran", "Q2", lambda r: r["verdict"] == "ran"),
    ("N2 root fstatat on allowed/ works", "N2", lambda r: r["observed"].get("root_fstatat_allowed_inner_control") == 0),
    ("N3 _WRITE_OK on allowed/ granted", "N3", lambda r: not any("control" in a for a in r["anomalies"])),
    ("N4 allowed/ cwd and log are Ran", "N4.allowed.cwd", lambda r: r["verdict"] == "ran"),
    ("N4 allowed/ log is Ran", "N4.allowed.log", lambda r: r["verdict"] == "ran"),
    ("P2.M1 /dev/null with --nonblock-stdio: flag set, both nonces written", "P2.M1.control", lambda r: r["verdict"] == "ran" and not r["anomalies"]),
    ("P2.M2 control: every launch Ran", "P2.M2.control",
     lambda r: r["verdict"] != "blocked" and len(p2_launches(r)) == 10 and all(l["verdict"] == "ran" for l in p2_launches(r))),
    ("P2.M4 root: control C clean", "P2.M4.root", lambda r: bool((r["observed"] or {}).get("steps")) and not any("control C" in a for a in r["anomalies"])),
    ("P2.M4 nobody: control C clean", "P2.M4.nobody", lambda r: bool((r["observed"] or {}).get("steps")) and not any("control C" in a for a in r["anomalies"])),
]


def report(recs, bad, title):
    oses = sorted({os_key(r) for r in recs})
    md = [f"# {title}", ""]
    md.append("## Runners")
    infos = {}
    for r in recs:
        if r["id"] == "RUNNER":
            infos.setdefault(os_key(r), r)
    for o in oses:
        r = infos.get(o)
        sw = (r["os"].get("sw_vers") if r else "-")
        md.append(f"- **{o}**: {sw}; build {r['os'].get('build') if r else '-'}; {r['os'].get('csrutil') if r else '-'}")
        if r:
            ob = r["observed"]
            md.append(f"  - uname: {ob.get('uname_v')}; model: {ob.get('hw_model')}; image: {ob.get('image_version')}; id: {ob.get('id')}")
    md.append("")
    # Q2 first: it decides how Q1 is read.
    q = by_question(recs)
    for name in sorted(q, key=lambda n: (0 if "known-gap" in n else 1, n)):
        ids = q[name]
        rows = []
        for i in sorted(ids):
            per = ids[i]
            any_r = next(iter(per.values()))
            exp = any_r.get("expected")
            diff = [("%s: %s" % (o, per[o].get("differs"))) for o in oses if o in per and per[o].get("differs") is not None]
            rows.append([i, exp if exp is not None else "-"] + [cell(per.get(o)) for o in oses] + [", ".join(diff) or "-"])
        md.append(f"## {name}")
        md += table(rows, ["id", "plan's expectation"] + oses + ["differs"])
        md.append("")
    md += pass2(recs, oses)
    md.append("## Controls")
    rows = []
    byid = {(r["id"], os_key(r)): r for r in recs}
    for label, rid, ok in CONTROLS:
        res = []
        for o in oses:
            r = byid.get((rid, o))
            try:
                res.append("-" if r is None else ("pass" if ok(r) else "FAIL"))
            except Exception as e:  # noqa: BLE001
                res.append(f"error: {e}")
        rows.append([label] + res)
    verdict_checks = (
        ("known-good sentinel is Ran", "verdict-ran", lambda r: r["verdict"] == "ran"),
        ("nonexistent cwd is Refused (exit 78)", "verdict-refused", lambda r: r["verdict"] == "refused"),
    )
    selftest_check = ("selftest (zombie, reparented, exec)", "selftest", lambda r: r["observed"].get("ok") is True)
    suspended_checks = (
        ("suspended-start: p0 is Ran (exited:0)", "suspended-ran", lambda r: r["verdict"] == "ran" and not r["anomalies"]),
        ("suspended-start: missing cwd is Refused (exited:78)", "suspended-refused", lambda r: r["verdict"] == "refused" and not r["anomalies"]),
    )
    jobs = {"b0": 0, "b0-size": 0, "devices": 0, "tcc": 0, "nfs": 0, "gap": 0, "ptmx": 0, "lifecycle": 1, "nfs-shipped": 2, "sandbox": 2}
    for job, kind in jobs.items():
        # kind 0: pass 1 shape; 1: also the new lifecycle; 2: selftest only (no launchd verdicts)
        checks = (() if kind == 2 else verdict_checks) + (selftest_check,) + (suspended_checks if kind == 1 else ())
        for label, suffix, ok in checks:
            res = []
            for o in oses:
                r = byid.get((f"SELF.{job}.{suffix}", o))
                res.append("-" if r is None else ("pass" if ok(r) else "FAIL"))
            rows.append([f"self-check [{job}] {label}"] + res)
    md += table(rows, ["control"] + oses)
    md.append("")
    md.append("## Anomalies")
    n = 0
    for r in recs:
        for a in r.get("anomalies") or []:
            md.append(f"- [{os_key(r)}] {r['id']}: {a}")
            n += 1
    for b in bad:
        md.append(f"- MALFORMED {b}")
        n += 1
    if n == 0:
        md.append("none")
    md.append("")
    md.append("## Refusal bookkeeping (X1)")
    rows, unsettled = [], 0
    for r in recs:
        if r["verdict"] in ("refused", "inconclusive"):
            fp = r.get("first_print")
            bad_fp = any("bookkeeping not settled" in a for a in r.get("anomalies") or [])
            unsettled += bad_fp
            rows.append([r["id"], os_key(r), r["verdict"], r.get("exit_source"), json.dumps(fp) if fp else "-"])
    md += table(rows, ["id", "os", "verdict", "exit source", "first print"]) if rows else ["no refused rows"]
    md.append("")
    md.append(f"Rows where the first print was unsettled: **{unsettled}**")
    md.append("")
    md.append("## The cap (B6)")
    limits = {}
    for r in recs:
        if r["id"] == "B6":
            limits[os_key(r)] = r["observed"]
    for o in oses:
        ob = limits.get(o)
        md.append(f"- {o}: " + ("not run" if ob is None else (f"limit {ob['limit']} bytes ({ob['limit_mib']:.3f} MiB)" if ob.get("limit") is not None else str(ob.get("note")))))
    nums = [ob["limit"] for ob in limits.values() if ob.get("limit") is not None]
    if nums:
        md.append(f"- larger of the two: **{max(nums)} bytes ({max(nums) / MIB:.3f} MiB)**; "
                  "a plist of that size is held in launchd's memory while it is parsed (memory use not measured)")
    return "\n".join(md) + "\n"


def emit(text, out_dir):
    os.makedirs(out_dir, exist_ok=True)
    with open(os.path.join(out_dir, "report.md"), "w") as f:
        f.write(text)
    s = os.environ.get("GITHUB_STEP_SUMMARY")
    if s:
        with open(s, "a") as f:
            f.write(text)
    print(text)


def main():
    a = sys.argv[1:]
    if len(a) == 2 and a[0] == "--check":
        recs, bad = load(os.path.join(a[1], "*", "*.json"))
        for b in bad:
            print("MALFORMED", b)
        print(f"{len(recs)} result files checked")
        return 1 if bad or not recs else 0
    if len(a) == 2 and a[0] == "report":
        recs, bad = load(os.path.join(a[1], "*", "*.json"))
        emit(report(recs, bad, "Step 0 probe: " + os.environ.get("PROBE_JOB", "job")), a[1])
        return 1 if bad else 0
    if len(a) == 3 and a[0] == "combine":
        recs, bad = load(os.path.join(a[1], "*", "*", "*.json"))
        emit(report(recs, bad, "Step 0 probe: all jobs"), a[2])
        return 1 if bad else 0
    print(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main())

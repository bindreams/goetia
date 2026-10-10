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
    for job in ("b0", "b0-size", "devices", "tcc", "nfs", "gap"):
        for label, suffix, ok in (
            ("known-good sentinel is Ran", "verdict-ran", lambda r: r["verdict"] == "ran"),
            ("nonexistent cwd is Refused (exit 78)", "verdict-refused", lambda r: r["verdict"] == "refused"),
            ("selftest (zombie, reparented, exec)", "selftest", lambda r: r["observed"].get("ok") is True),
        ):
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

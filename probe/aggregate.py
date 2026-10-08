#!/usr/bin/env python3
"""Cross-row classification for the launchd writable-dirs probe. Throwaway; stdlib only.

Usage: aggregate.py RESULTS_DIR [--expect-f29]
Writes RESULTS_DIR/aggregate.json and a Markdown table to $GITHUB_STEP_SUMMARY (or stdout).
Exits 1 only when an expected results file is missing or malformed.
"""
import json
import os
import sys

ALL = ["F1c", "F2c", "F25c", "F5l", "F19a", "F6l", "F7l", "F12l", "F26l", "F21b", "F22l", "F23l",
       "F24l", "F28l", "F29l", "F14g", "F18g", "F20g", "F16l", "F17l"]
O_ACCMODE, O_WRONLY, O_RDWR, O_NONBLOCK, O_APPEND = 3, 1, 2, 4, 8


def parse_ready(line):
    if not line:
        return None
    head, _, cwd = line.partition(" cwd=")
    out = {"cwd": cwd}
    for tok in head.split()[1:]:
        k, _, v = tok.partition("=")
        out[k] = v
    for fd in ("fd0", "fd1", "fd2"):
        v = out.get(fd, "")
        if v.startswith("err:"):
            out[fd] = {"err": int(v[4:])}
            continue
        parts = v.split(",")
        d = {"getfd": int(parts[0]), "getfl": int(parts[1])}
        if len(parts) == 3:
            d["staterr"] = parts[2]
        elif len(parts) == 6:
            d.update(dev=int(parts[2]), ino=int(parts[3]), mode=parts[4], rdev=int(parts[5]))
        fl = d["getfl"]
        d["accmode"] = {0: "RDONLY", O_WRONLY: "WRONLY", O_RDWR: "RDWR"}.get(fl & O_ACCMODE, "?")
        d["append"] = bool(fl & O_APPEND)
        d["nonblock"] = bool(fl & O_NONBLOCK)
        out[fd] = d
    out["acc"] = [int(x) for x in out.get("acc", "").split(",") if x != ""]
    return out


def need_for(fd):
    """Rights an existing-file open authorizes (vfs_subr.c:8440-8452)."""
    s = ""
    if fd["accmode"] in ("RDONLY", "RDWR"):
        s += "R"
    if fd["accmode"] in ("WRONLY", "RDWR"):
        s += "A" if fd["append"] else "W"
    return "".join(sorted(s, key="RWA".index))


def same(fd, obs):
    return bool(fd and obs and "dev" in fd and obs.get("dev") == fd["dev"] and obs.get("ino") == fd["ino"])


def content_change(before, after):
    b = (before or {}).get("content")
    a = (after or {}).get("content")
    if a is None:
        return "no-regular-file-after"
    if b is None:
        return "created"
    if a == b:
        return "unchanged"
    if a.startswith(b):
        return "appended"
    if len(a) < len(b):
        return "truncated"
    return "overwritten"


def mech_bools(rec):
    out = {}
    for m in rec.get("mechanisms") or []:
        vals = []
        for v in m["verdicts"]:
            if "errno" in v:
                vals.append(v["errno"] == 0)
            else:
                vals.append(None)  # mechanism-failed: excluded
        out[m["mech"]] = {"granted": vals, "groups": m.get("groups"), "raw": m["verdicts"]}
    return out


def classify(launchd, root, mech):
    if launchd is None or root is None or mech is None:
        return None
    disc = root != mech
    if mech == launchd and root == launchd:
        c = "both"
    elif mech == launchd:
        c = "account"
    elif root == launchd:
        c = "root"
    else:
        c = "neither"
    return {"class": c, "discriminates": disc}


def main():
    d = sys.argv[1]
    expect_f29 = "--expect-f29" in sys.argv
    problems, rows = [], {}
    for rid in ALL:
        if rid == "F29l" and not expect_f29:
            continue
        p = os.path.join(d, f"{rid}.json")
        try:
            with open(p) as f:
                rows[rid] = json.load(f)
        except Exception as e:  # noqa: BLE001 - any failure is "missing or malformed"
            problems.append(f"{rid}: {e}")
    extras = {}
    for name in ("selftest", "exfat"):
        try:
            with open(os.path.join(d, f"{name}.json")) as f:
                extras[name] = json.load(f)
        except Exception as e:  # noqa: BLE001
            problems.append(f"{name}: {e}")

    # Flag model from the positive controls (F7l, else F19a/F24l).
    model = {}
    for src in ("F7l", "F19a", "F24l"):
        r = parse_ready((rows.get(src) or {}).get("ready"))
        if r and isinstance(r.get("fd1"), dict) and "accmode" in r["fd1"]:
            model = {"source": src, "fd1": need_for(r["fd1"]), "fd2": need_for(r["fd2"]),
                     "fd1_flags": r["fd1"], "fd2_flags": r["fd2"]}
            break

    table = {}
    for rid, rec in rows.items():
        t = {"anomalies": rec.get("anomalies"), "outcome": rec.get("outcome"), "status": rec.get("status")}
        ready = parse_ready(rec.get("ready"))
        t["ran"] = ready is not None
        mb = mech_bools(rec)
        targets = rec.get("targets") or []
        kind = rec.get("kind")
        launchd = {}  # need-name -> bool
        if ready:
            t["fds"] = {k: ready.get(k) for k in ("fd0", "fd1", "fd2")}
            t["writes"] = {"w1": ready.get("w1"), "w2": ready.get("w2")}
        if kind in ("cwd", "cwd-and-log"):
            cs = rec.get("cwd_stat") or {}
            ok = False
            if ready:
                dot = ready.get("dot", "")
                if not dot.startswith("err"):
                    dev, ino = (int(x) for x in dot.split(","))
                    ok = cs.get("dev") == dev and cs.get("ino") == ino
                else:
                    k = rec.get("cwd_kernel") or {}
                    ok = cs.get("dev") == k.get("dev") and cs.get("ino") == k.get("ino")
                t["cwd_seen"] = ready.get("cwd") or rec.get("cwd_kernel")
            launchd["X"] = ok
            t["ran_elsewhere"] = bool(ready) and not ok
        log_after = rec.get("log_after")
        if rec.get("log"):
            obs = rec.get("target_after") if rid == "F23l" else log_after
            if rid == "F28l" or rid == "F29l":
                obs = rec.get("log_before")  # the FIFO's identity
            o1 = same(ready and ready.get("fd1"), obs)
            o2 = same(ready and ready.get("fd2"), obs)
            if rid == "F24l":
                dn = rec.get("dev_null") or {}
                fd1 = (ready or {}).get("fd1") or {}
                o1 = fd1.get("rdev") == dn.get("rdev") and fd1.get("ino") == dn.get("ino") \
                    and (ready or {}).get("w1", "-1").split(",")[0] not in ("-1", "0")
                o2 = None
            t["opened_fd1"], t["opened_fd2"] = o1, o2
            t["content"] = content_change(rec.get("log_before"), obs if rid == "F23l" else log_after)
            t["log_after"] = {k: (obs or {}).get(k) for k in ("lstat_type", "uid", "gid", "mode")}
            if rid in ("F28l", "F29l"):
                t["fifo_read"] = rec.get("log_reader") or rec.get("released_reader")
            t["ran_without_log"] = bool(ready) and not o1
            if kind in ("log-create", "cwd-and-log") or rid == "F23l":
                launchd["WX"] = o1
            elif model:
                launchd[model["fd1"]] = o1
                if o2 is not None:
                    launchd.setdefault(model["fd2"], o2)
        if kind == "groups" and ready:
            for i, n in enumerate(rec.get("job_needs") or []):
                if i < len(ready["acc"]):
                    launchd[n["need"]] = ready["acc"][i] == 0
        t["launchd"] = launchd
        # Compare booleans per need (L1), excluding F22l (L2).
        cmp = {}
        if rid != "F22l":
            root = mb.get("root", {}).get("granted", [])
            for i, tg in enumerate(targets):
                L = launchd.get(tg["need"])
                if L is None:
                    continue
                for m, v in mb.items():
                    if m == "root":
                        continue
                    c = classify(L, root[i] if i < len(root) else None, v["granted"][i])
                    if c:
                        cmp.setdefault(tg["need"], {})[m] = c
                cmp.setdefault(tg["need"], {})["_root_granted"] = root[i] if i < len(root) else None
                cmp[tg["need"]]["_launchd"] = L
        t["compare"] = cmp
        t["facts"] = rec.get("facts")
        t["groups"] = {m: v["groups"] for m, v in mb.items()}
        table[rid] = t

    decisions = {}
    # D1/D3/D4: who, per kind, per mechanism: all discriminating rows must class account|both.
    kinds = {"cwd": ["F1c", "F2c", "F19a"], "log-create": ["F5l", "F19a"],
             "log-existing": ["F6l", "F7l", "F12l", "F26l"], "groups": ["F14g", "F18g", "F20g"]}
    for k, ids in kinds.items():
        per = {}
        for rid in ids:
            for need, ms in (table.get(rid, {}).get("compare") or {}).items():
                if k == "cwd" and need != "X" or k == "log-create" and need != "WX":
                    continue
                for m, c in ms.items():
                    if m.startswith("_"):
                        continue
                    per.setdefault(m, []).append((rid, need, c["class"], c["discriminates"]))
        verdict = {}
        for m, lst in per.items():
            disc = [x for x in lst if x[3]]
            acct = all(x[2] in ("account", "both") for x in disc)
            root = all(x[2] in ("root", "both") for x in disc)
            verdict[m] = {"as_account": acct and bool(disc), "as_root": root and bool(disc),
                          "discriminating_rows": disc, "all": lst}
        decisions[k] = verdict
    decisions["D2_ran_elsewhere"] = {r: table.get(r, {}).get("ran_elsewhere") for r in ("F1c", "F25c")}
    decisions["D5_ran_without_log"] = {r: table.get(r, {}).get("ran_without_log")
                                       for r in ("F5l", "F6l", "F21b", "F22l")}
    decisions["flag_model"] = model
    decisions["D8"] = {r: table.get(r, {}).get("compare") for r in ("F16l", "F17l")}
    decisions["D9"] = extras.get("exfat")
    decisions["selftest"] = extras.get("selftest")

    out = {"problems": problems, "rows": table, "decisions": decisions}
    with open(os.path.join(d, "aggregate.json"), "w") as f:
        json.dump(out, f, indent=1, default=str)
    md = ["| row | ran | outcome | opened fd1/fd2 | content | launchd | anomalies |", "|---|---|---|---|---|---|---|"]
    for rid in ALL:
        t = table.get(rid)
        if not t:
            continue
        md.append(f"| {rid} | {t['ran']} | {t['outcome'] or t['status']} | {t.get('opened_fd1')}/{t.get('opened_fd2')} "
                  f"| {t.get('content')} | {t['launchd']} | {len(t['anomalies'] or [])} |")
    md.append("")
    md.append("```json")
    md.append(json.dumps({k: v for k, v in decisions.items() if k in ("flag_model", "D2_ran_elsewhere",
                                                                     "D5_ran_without_log")}, default=str))
    md.append("```")
    if problems:
        md.append(f"**problems:** {problems}")
    text = "\n".join(md) + "\n"
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(text)
    print(text)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())

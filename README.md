# Goetia

A cross-platform CLI and library that installs system daemons described in a
`goetia.yaml` manifest as native services: systemd on Linux, launchd on
macOS, and the Service Control Manager on Windows. A daemon is a service, which
keeps running, or a [task](#tasks), which runs a program to completion on a
schedule or by hand (systemd timers, launchd calendar jobs and Windows Task
Scheduler).

## Status

Pre-1.0. See the [releases page](https://github.com/bindreams/goetia/releases)
for what has actually shipped.

Everything documented below — the manifest's interpolation syntax, the
`.env` grammar, the `--json` document (a task's fields included), and the exit
codes — is **pre-1.0 and may still change** between releases. Pin a specific released version, or a
specific commit if working ahead of one.

On macOS goetia requires **15+**: nothing older can be tested, since GitHub
offers no runner for it. On Linux it requires **kernel 5.6+**,
with `pidfd_open` and `openat2` not blocked by a seccomp profile or an LSM: the
process library goetia runs `systemctl` with, and holds a daemon's process with
for `kill`, needs both. It also requires **systemd 243+**
(2019), and refuses to install, uninstall, start, stop, restart, enable or
disable anything on an older one, before writing or running anything. It checks both the running systemd and the
`systemctl` client, which is what parses `--show-transaction`. A running
systemd it cannot ask is refused. Generated units use
`Type=exec` (240+), which is what lets a start report a missing executable or
user as a failure. An older systemd does not reject the directive: it logs that it
cannot parse it and runs the unit as `Type=simple`, silently losing exactly
that. Every `start` and `stop` runs `systemctl --show-transaction` (242+), and
arming a task's timer runs `systemctl clean` on it (243+) to clear its stamp,
which is how arming a task again makes up no run missed while goetia's
`disable` had disarmed it.

On Linux goetia finds `systemctl`, `journalctl` and `busctl`, and on macOS
`launchctl`, by searching the caller's `PATH`. Every `PATH` entry before the
one that holds the tool must be one the calling account can search: an entry it
cannot search stops the search with an error that names the entry, even when a
later entry holds the tool (for example after switching user while keeping the
old `PATH`). A `PATH` must be set: with none the tool is not found, and there is
no default search path. The search takes the first file of that name on
`PATH`, whatever its mode: one that cannot be executed fails the call, and the
search does not go on. To recover, remove or fix the entry, or run with a
system `PATH` such as `/usr/bin:/bin:/usr/sbin:/sbin`.

goetia does not manage systemd offline or from a chroot: it works only
through the running systemd manager of the system it runs on. Where it finds
`SYSTEMD_OFFLINE` set to a true value; a `/` of goetia's own rather than the
system's, seen in `/proc/1/root` — one other than PID 1's root: a chroot, or
a container sharing the host's PID namespace — or, only where that cannot be
read, in the mount tables: a `/` whose mount is none of PID 1's, or one with
no mount at all, which only `chroot(2)` leaves and which goetia's own table
shows without PID 1's; `/run/systemd/system` missing; or `systemctl`
reporting a chroot — a verb that would reach systemd exits `1` before it
writes or sends anything, with one message naming the first of these it
found. No switch under either prefix systemd gives this binary can turn that
last one off: every inherited `SYSTEMD_*` and `SYSTEMCTL_*` variable is
removed from the `systemctl` goetia runs, and only the three goetia sets
itself are given back, so nothing in those two namespaces — including a
switch a newer systemd adds — can stop it detecting a chroot goetia could not
see for itself, or send it to a manager that is not this root's. A variable
under neither prefix is passed through as it arrived. `install` always
reaches it; the other verbs only for a daemon of goetia's, and they answer
from files alone otherwise:

- `uninstall`, `start`, `stop`, `kill`, `restart`, `enable` and `disable` refuse
  an id goetia finds its own, and answer any other as anywhere else — for
  one with nothing installed, `uninstall` exits `0`, "nothing to do", and
  the others `1`, "not installed".
- `status <id>` refuses an id of goetia's whose unit decodes, the only kind
  whose live state it reads, and answers any other as anywhere else.
- `status` and `list` read the live state of every such daemon, so with one
  installed they refuse. With none, they answer from files: with nothing
  installed, an empty listing, exit `0`.
- `show <id>` without `--file` renders what `list` reads, so it refuses
  while any such daemon is installed, whether or not it is that id. With
  none, an id with nothing installed is "not installed", exit `1`.

`install --dry-run`, `diff` and `show --file` never reach systemd, and work
there as anywhere else.

## Verifying a download

Every release ships `SHA256SUMS` and one `goetia.sigstore.json` — a Sigstore
bundle attesting GitHub build provenance for **the five archives and nothing
else**. `goetia.sigstore.json` is the thing to verify against; `SHA256SUMS`
is a convenience for comparing a download by hand and carries no attestation
of its own. A `SHA256SUMS` match is a sanity check, not a substitute for the
steps below.

Verification needs only [`sigstore`](https://pypi.org/project/sigstore/)
from PyPI (`pip install sigstore`) — no `gh` CLI required. A bundle verifies
against exactly one file at a time, so checking all five archives is a loop,
one invocation per archive, for example:

```
python -m sigstore verify github --bundle goetia.sigstore.json \
  --repository bindreams/goetia --ref refs/heads/main \
  --name "Draft Release" --trigger workflow_dispatch \
  --cert-identity https://github.com/bindreams/goetia/.github/workflows/draft-release.yaml@refs/heads/main \
  goetia-x86_64-unknown-linux-musl.tar.xz
```

Repeat with each of the other four archive names in place of the last
argument. None of the five pinned values above embeds the release version,
so this command does not need updating between releases.

`sigstore` decides success or failure by **exit status**, not by anything it
prints: on success it writes `OK: <file>` to **stderr** and the verified
in-toto statement to **stdout**, so a script that greps stdout for `OK`
never matches and silently reports success on every run, including failed
ones. Check `$?` (or your shell's equivalent), not the output.

Two flags are easy to get wrong:

- `--cert-identity-regexp` is cosign's flag. `sigstore` (the PyPI package
  used here) does not have it, and passing it aborts before anything is
  verified — but the exact error depends on where you put it. Placed before
  the file argument, as every flag is in the example above, argparse
  consumes the pattern as the file argument instead and aborts with
  `argument FILE_OR_DIGEST: invalid file_or_digest value: '<pattern>'`;
  placed after the file argument, it aborts with `unrecognized arguments`.
  Either way, nothing is verified.
- `--repository` alone is not a pin on which workflow produced the bundle —
  it accepts a bundle minted by any workflow, on any ref, in that
  repository. Passing `--name`, `--ref`, `--trigger`, and `--cert-identity`
  alongside it, as in the example above, is what actually pins the bundle
  to this project's release pipeline.

This command also only proves the archive's bytes came from goetia's release
pipeline — not that it's the platform its filename claims. Sigstore matches
the bundle against the file's digest alone; it does not check the file's
name against the bundle's subjects. Renaming one verified archive to another
archive's filename (e.g. `goetia-aarch64-apple-darwin.tar.xz` renamed to
`goetia-x86_64-unknown-linux-musl.tar.xz`) still verifies successfully. If
that distinction matters to you, also confirm the archive's top-level
directory matches its filename:

```
tar tf goetia-x86_64-unknown-linux-musl.tar.xz | head -1
unzip -Z1 goetia-x86_64-pc-windows-msvc.zip | head -1
```

Each prints a path that must start with `goetia-<target>/` for the target
the filename claims — either that directory entry on its own
(`goetia-x86_64-pc-windows-msvc/`) or the first file inside it
(`goetia-x86_64-pc-windows-msvc/goetia.exe`), depending on how the archive
was written, so check the prefix rather than an exact match.

Use `unzip -Z1`, not `unzip -l`: `-l` starts with a banner reading
`Archive:  <filename>`, which echoes back the name you typed. That name is
the untrusted claim being checked, so a check that reads it proves nothing.

## The manifest

A `goetia.yaml` describes one daemon: its top level is the daemon's fields.
`id:` is required and is the name of the installed service. `install` and
`diff` refuse, even with `--force`, when the service manager finds a different
daemon's service at that id: on Windows and case-insensitive macOS, `FRPC` finds
an installed `frpc`. Exit `1`; rename the daemon or uninstall the other one.

```yaml
id: frpc
command: [bin/frpc, -c, frpc.toml]
restart: on-failure
backend-specific:
  scm: { restart: on-failure }
```

### `type`, `protocol` and `schedule`

`type` is `service` (the default) or `task`. A service has `restart`,
`restart-delay` and `protocol`; a task has `schedule`. A field of the other
type is refused, naming the field. `type` cannot be set under
`backend-specific:`.

`protocol` is `plain` (the default) or `native`. A `plain` service is a command
goetia runs; on Windows it runs under goetia's shim. A `native` service is a
program that speaks to the service manager itself. Earlier releases wrote
these as `type`'s `simple` and `managed`; both are now refused, with a message
naming the `protocol` that replaces each.

`schedule` is five-field cron, `minute hour day-of-month month day-of-week`,
with Jenkins's `H`. Each field is a comma list of terms: `*`, `n`, `a-b`,
`*/n`, `a-b/n`, `H`, `H(a-b)`, `H/n` or `H(a-b)/n`. Names (`mon`), `@` macros,
`L`, `W`, `#` and `?` are refused. `H` picks one value per daemon and host, from
a hash of the host name, the id and the field's position, so that tasks do not
all fire at minute 0. As in Vixie cron, when day-of-month or day-of-week
starts with `*` both must match; otherwise either may, and a field starting
with `H` counts as restricted. A schedule that starts with `*` must be quoted in
YAML: `schedule: "*/15 * * * *"`. A task without a `schedule` is started only by
hand. A schedule is read in the machine's local time zone. Where a platform
cannot hold a schedule, `install` refuses it, naming the limit; what each
platform refuses, and what each does at a daylight saving change, is in
[The schedule, per platform](#the-schedule-per-platform).

`-f` takes a manifest file, or a directory holding `goetia.yaml`. Several
daemons are several manifests, each installed with its own
`goetia daemon install -f <file>`; manifests in one directory share its
`.env`.

### `stop-action`

`stop-action` says how a stop asks the daemon to exit, and how long the manager
waits for it to. Both forms below mean the same; the second spells out the
defaults:

```yaml
stop-action: terminate # short form of:
stop-action:
  send-signal: terminate # the default
  timeout: 30s # whole seconds, 1s to 24h; `null` for no timeout
```

Either key may be left out, and `stop-action: {}` is the default. A stop sends
`send-signal`; a daemon that has not exited after `timeout` is ended by the
manager. `timeout: null` is the one `null` a manifest may write, and means no
timeout: the manager waits as long as the platform allows, which launchd
refuses (see below). A `timeout` that is not a whole number of seconds, or is
under `1s` or over `24h`, is refused.
`stop-action` applies to a service and to a task alike (a task's stop ends the
run in flight), and can be set per backend, leaf by leaf: a `backend-specific`
entry that writes only `timeout` keeps the base's `send-signal`.

`send-signal` is one of:

| Value                              | systemd                | launchd            | Windows                            |
| ---------------------------------- | ---------------------- | ------------------ | ---------------------------------- |
| `terminate` (default)              | `SIGTERM`              | SIGTERM            | Ctrl+C                             |
| `interrupt`                        | `SIGINT`               | refused            | Ctrl+C                             |
| `quit`                             | `SIGQUIT`              | refused            | Ctrl+Break                         |
| `kill`                             | `SIGKILL`              | `ExitTimeOut` of 1 | ends the daemon at once            |
| `sigusr1`, `SIGRTMIN+3`, ...       | that signal (any case) | refused            | refused                            |
| `ctrl-c`, `ctrl-break`, `wm-close` | refused                | refused            | that event (`wm-close`: see below) |

A value a platform cannot send is refused, naming the platform, and never turned
into another signal: `sigterrm` is an error, not a `SIGTERM`. `SIGTERM`, `SIGINT`,
`SIGQUIT` and `SIGKILL` (any case, `SIG` optional) are `terminate`, `interrupt`,
`quit` and `kill`, with every rule of those. Any other native signal is
checked against the names systemd knows (`SIGHUP` to `SIGSYS`, `SIGRTMIN` to
`SIGRTMIN+30`, `SIGRTMAX-30` to `SIGRTMAX`: systemd takes an offset of 0 to
`SIGRTMAX - SIGRTMIN`, which is 30 with glibc), on every host.

`kill` has nothing to answer and nothing to wait for: its `timeout` is `null`,
and any other value is refused. `quit` ends a systemd unit as failed, since
`SIGQUIT` dumps core, where Windows records the stop as clean: each platform
records the end as it does.

- **systemd** writes `KillSignal=` and `TimeoutStopSec=` into every unit
  (`infinity` for `null`), and sends `SIGKILL` after the timeout. `kill` writes
  `KillSignal=SIGKILL` and no `TimeoutStopSec=`, so systemd's own default still
  bounds a process the kernel holds.
- **launchd** writes `ExitTimeOut`. launchd sends `SIGTERM` and nothing else,
  and `SIGKILL` once `ExitTimeOut` is over. **macOS 26.4 and later cut any
  `ExitTimeOut` to 60 s** (measured; Apple does not document it), so launchd
  refuses a `timeout` over `60s`, and `timeout: null` for any signal but `kill`,
  on every macOS version, naming the cap: it would be cut on some and honoured
  on others. `kill` waits for nothing and has no timeout to refuse. launchd
  sends `SIGTERM` to the job's whole process group, but `SIGKILL` only to the
  job's process, so a descendant that ignores `SIGTERM` outlives a stop. `kill`
  writes an `ExitTimeOut` of `1`: launchd
  sends `SIGTERM`, and `SIGKILL` a second later. `0`, which `launchd.plist(5)`
  documents as no limit, ends the job at once on macOS 15.7 (Intel) and 26
  (measured), against that documentation. `1` does not depend on what `0` does.
  `ExitTimeOut` applies only when launchd itself boots a job out, which a task's
  `stop` never does: goetia ends a task's run itself, with `SIGKILL` at once for
  `kill` and otherwise `SIGTERM`, then `SIGKILL` if the run is still going when
  `timeout` is over. Both signals go through a hold on the process that
  `launchctl procinfo` names as the job's, never by pid or by the label, so
  neither reaches a run that began after, or a process that reused the pid; and
  they go to that process alone, not to its process group (measured:
  `launchctl kill` signals the leader only). The `timeout` is the grace the manifest
  configures, and it is kept only while goetia waits: on a launchd task alone, a
  `stop --timeout` shorter than the `timeout`, or `--timeout 0`, ends the wait
  after the `SIGTERM`, nothing sends the `SIGKILL`, and a run that ignores
  `SIGTERM` keeps going until `goetia daemon kill <id>` ends it; the same holds
  when the wait is spent just as the `timeout` ends, and the error says whether
  a `SIGKILL` had gone out. The wait does not count the time a queued start took
  to fork.
- **Windows** applies it to a `protocol: plain` service. A stop, from `stop`, `restart`, `uninstall` or SCM
  itself (`sc stop`), sends the daemon the console event `send-signal` names (Ctrl+C for `terminate`,
  `interrupt` and `ctrl-c`; Ctrl+Break for `quit` and `ctrl-break`), through a copy of the shim host that
  attaches to the daemon's console, and then waits for the daemon's whole process tree to exit for up to
  `timeout` (as long as it takes for `null`) before it ends what is left. A launcher whose worker needs time
  to exit, and whatever a daemon that honoured the event left running, have the `timeout` too. A daemon that
  had exited when the stop looked is not asked, and what it left in the job is ended at once; one that exits
  as it is being asked is waited for like one that was asked. `kill` sends nothing and ends
  the daemon at once. `wm-close` still does too: it sends nothing yet. A stop that asks writes its lines to the
  daemon's log (the file `logs` names, or the default log when it has none) and to the Windows Event Log: one
  before the first ask, naming the event and how long the daemon and what it started are given, and one saying
  how the stop ended: that they exited within the wait, or (a warning) that the daemon's process tree had not
  been seen to exit when the `timeout` was over and is being ended. The limits:
  - The event reaches every process that shares the daemon's console, not only the daemon.
  - A program with no console (a GUI program) cannot be sent the event, so it is ended only when its
    `timeout` is over; write `send-signal: kill` for one. A stop right after a `start` finds the daemon
    without its console for a few milliseconds: the shim asks again until the daemon has it, exits, or the
    `timeout` is over. The pause between asks doubles from 25 ms to 1 s, so with `timeout: null` a program
    that never has a console is asked again, about once a second, until `goetia daemon kill`.
  - `stop` and `restart` wait the `timeout` plus 10 s, as above. `uninstall` waits up to the `timeout`
    too, holding the machine-wide install lock as it does for any stop, so an `install` or `uninstall` of
    another daemon waits behind it. For `timeout: null` and a tree that never exits it waits until
    `goetia daemon kill` ends the daemon; a `stop --timeout` ends only goetia's own wait.
  - The service reports `STOP_PENDING` with a wait hint that covers the `timeout` and the teardown (the
    largest value the field holds for `null`); SCM does nothing when a hint runs out. While the stop waits for
    the daemon's tree it reports `STOP_PENDING` again every 5 s, with the checkpoint one higher; nothing is
    reported while the ask itself is being made.
  - **At a system shutdown** a `protocol: plain` service is stopped as `stop` stops it: it accepts `PRESHUTDOWN`,
    and `install` sets its pre-shutdown time from the `stop-action`: the `timeout` plus 10 s, the largest value
    for `null`, 10 s for `kill`. A `protocol: native` service's is its own, which `install` leaves alone, except
    that changing a service from `plain` to `native` puts it back to Windows' default of 10 s, once. goetia
    promises that Windows waits for the service at least that long, and ends one whose checkpoint stays put for about
    45 s, which is why the stop reports one that moves (both measured in Server Core containers of Windows
    Server 2025, build 26100; not measured on other machines).
    - A service whose `timeout` is `null` and whose daemon does not exit holds the shutdown for as long as the
      daemon runs (still waited for at 900 s when measured), and the waits of services that do not exit add up.
      Write a finite `timeout`.
    - A `stop`, `restart` or `uninstall` of a service that is still in flight when the shutdown begins is cut
      within about two seconds, whatever its `timeout`: a limit of the platform.
    - A task's run at a shutdown is as it was: see [Stopping and killing a task](#stopping-and-killing-a-task).
  - A program that hosts the shim itself calls `goetia::shim::init_host()` first in its `main` (see
    [Shipping the shim from a library](#shipping-the-shim-from-a-library)); a host that does not is refused when the service starts, with an error that names the
    call. `send-signal: kill` and `wm-close` need no such host.

  A task is stopped by Task
  Scheduler, which decides the signal and the wait, so a task refuses a
  `send-signal` or `timeout` that is not the default; set it under `backend-specific` for systemd and launchd. A
  `protocol: native` service handles SCM's own stop itself, so it refuses
  `send-signal`; its `timeout` still sets the wait of `stop` and `restart`.

With no flag, `goetia daemon stop` waits for the daemon's `timeout` plus 10 s (the stop's own teardown): without
bound for `null`, and 10 s for `send-signal: kill`. `goetia daemon restart` waits that long for its stop, and 10 s
for its start. `--timeout` and `--no-timeout` override both, and then bound the whole restart. A daemon whose
definition cannot be read has no stop action to take the wait from: `stop` and `restart` with no flag fail,
naming why, and with `--timeout` or `--no-timeout` they run. `goetia daemon show` prints the leaves that differ
from the default.

**Upgrading.** A stop used to wait 90 s on systemd (its own default), 5 s on
launchd and nothing on Windows; the default is now `30s` on systemd and launchd
for a daemon installed by this version. Reinstall every daemon an older goetia
installed, with `goetia daemon install`:

- It keeps its old unit, with no `TimeoutStopSec=` or `ExitTimeOut`, until
  `goetia daemon install` writes it again. Its definition reads as the default
  `30s`, so until then `stop` and `restart` wait for 30 s plus the margin while
  the unit waits for systemd's 90 s or launchd's 5 s.
- A daemon installed by this version is unreadable to an older goetia, which
  reports "unsupported blob schema 2" (the definition's schema is now 2; this
  version still reads schema 1).

## Interpolation

Fields of `goetia.yaml` may reference variables defined in a `.env` file
sitting beside the manifest.

### Syntax

Exactly three forms are accepted:

| Form               | Meaning                                                                                       |
| ------------------ | --------------------------------------------------------------------------------------------- |
| `${NAME}`          | The value of `NAME` from `.env`. If `NAME` is not defined at all, this is an error naming it. |
| `${NAME:-default}` | `NAME`'s value if it is defined and non-empty; otherwise `default`.                           |
| `$$`               | A literal `$`.                                                                                |

**Every other `$` is an error.** A `$` not immediately followed by `{` or by
another `$` is rejected, naming its byte offset — a typo like `$HOME` cannot
silently become the literal text `$HOME` in a generated service artifact.
This is one of three places a manifest that was previously accepted can now
fail — the others are
[an explicit `null`](#an-explicit-null-is-not-a-way-to-unset-a-field) and
[an `env` key written twice](#an-env-key-may-not-repeat):
**a literal `$` must be written `$$`**, so a `command` argument written
`$ARGS` becomes `$$ARGS`. Text that already looked like a reference breaks
differently but needs the same fix: a literal `${A}` now fails with
``no value for `A` `` rather than being passed through, and must be written
`$${A}`.

Further rules:

- `${NAME:-default}` falls back when `NAME` is **unset _or_ set to the empty
  string**. The unset-only shell form `${NAME-default}` is not supported;
  `-` is rejected as an invalid character in a variable name.
- `default` may not contain a `$` in any form — no nested substitution, no
  escape. This is checked unconditionally, even when `NAME` is set and
  `default` is never used, because grammar validity does not depend on which
  branch wins.
- `default` ends at the first `}`, with no escape for a literal `}` inside
  it: `${A:-{x}}` reads as `default = "{x"` and leaves the second `}` as
  ordinary text, so a default containing a closing brace cannot be written.
- `NAME` matches `^[A-Za-z_][A-Za-z0-9_]*$` — the same charset a `.env` name
  must match, so a name that can be assigned can always be referenced and
  vice versa.
- **Substituted text is never rescanned.** A variable whose value is the
  literal text `${OTHER}` is emitted as-is.

### Where values come from

`<manifest dir>/.env` is the **only** source. The process environment
deliberately does not participate, in either direction.

That is not a gap. Goetia stores a resolved spec inside each installed
artifact and re-derives it later, from the same manifest and `.env`, to
detect drift — and since `install` runs elevated while `diff` and `show` do
not, a variable source that consulted the process environment would report
spurious drift on an artifact nobody touched, depending only on who ran the
command.

The same elevation split has a file-permission consequence: a `.env`
readable only by root makes `diff` and `show -f` fail on a manifest that
`install` handles. Plain `show` is unaffected — it reads no manifest at all,
so it never opens `.env` (guarantee 3 below). A manifest containing **no `$` at all never reads
`.env`**, so an unrelated (or root-only) `.env` beside such a manifest is
harmless.

### `.env` grammar

A deliberate strict subset of docker-compose's:

- The file is read as bytes. A leading UTF-8 BOM is stripped; a UTF-16 or
  UTF-32 BOM is rejected with an error naming the encoding. Everything else
  is decoded as UTF-8.
- Line terminators are `\n` and `\r\n`.
- A line that is entirely whitespace, or whose first non-whitespace
  character is `#`, is ignored.
- Leading whitespace before a name is allowed and trimmed.
- An optional `export` prefix is recognised only when followed by
  whitespace. `export=1` therefore assigns the variable named `export`,
  exactly as a shell would.
- `NAME=VALUE`, where `NAME` matches `^[A-Za-z_][A-Za-z0-9_]*$`.
- `VALUE` is one of: single-quoted (literal up to the closing `'`, no
  escapes); double-quoted (only `\\` and `\"` are decoded); or unquoted
  (trailing whitespace trimmed, a `#` preceded by whitespace starts a
  comment, a `#` not preceded by whitespace is part of the value).
- A quoted value must close on the same line; multi-line values are not
  supported. Anything other than whitespace or a `#` comment after a closing
  quote is an error.
- A missing `.env` is not an error and yields no variables. Only "file not
  found" means absent — every other IO failure (a directory at the `.env`
  path, a permission denial) is reported.

Four rules diverge from docker-compose on purpose:

1. **Values are fully literal.** `$`, `${...}`, and `$$` inside a `.env`
   value are ordinary characters. Expansion happens in the manifest, not
   here.
2. **Whitespace around `=` is an error**, not trimmed: write `A=1`, not
   `A = 1`. python-dotenv accepts `A = 1` and a shell does not, so accepting
   it would let one file mean two things to two readers. The single
   exception is `A= ` — an `=` followed only by whitespace to end of line —
   which means `A=`, an empty value, because an unquoted value's trailing
   whitespace is trimmed first.
3. **Every backslash escape but `\\` and `\"` is an error**, including
   `\n`, `\r`, and `\t`. Decoding one would only ever produce a control
   character, which goetia rejects downstream anyway; rejecting it here is
   the same refusal with a `.env` line number. A literal backslash-then-`n`
   is written `"a\\nb"`.
4. **A duplicate name is an error**, naming the line it first appeared on,
   rather than compose's last-wins. A repeated key in a hand-edited `.env`
   is a mistake, not an override.

### What is and is not interpolable

Every manifest field is interpolable except `id`, mapping keys and
`user.id`:

- **`id`** is never interpolated, because it names the installed service. A
  `$` in it is an error: an interpolated id could change which service this
  manifest installs.
- **`env` names** are map keys, so an interpolated name could silently
  overwrite another entry. `env` _values_ interpolate
  normally.
- **`user.id`** is not interpolable. Under `user: {id: 1000}` the value is a
  YAML number, so there is nothing a string could be substituted into; a
  `user.id` string carrying a `$` is rejected outright, because a
  substituted value is always a string and would therefore be read as a
  Windows SID rather than a uid. Write the uid literally, or use
  `user: <name>` — `user.name` interpolates.

Two consequences of _when_ substitution runs — after the YAML parse, on
typed fields:

- A `${...}` inside a YAML comment is not a value, so it is never seen and
  never an error.
- **A substituted value can never change the document's shape.** It cannot
  introduce a key, a list element, or a nesting level; it can only replace
  the contents of a string that the parser already produced. Every YAML
  diagnostic — line, column, duplicate key, unknown field — likewise refers
  to the file exactly as written.

### Security: do not put secrets in `.env` for use in `env:`

Interpolated values are written **verbatim** into the generated artifacts.
Goetia `chmod`s both the systemd unit and the launchd plist to `0644`, which
is world-readable, and `goetia daemon show` and `goetia daemon diff` render
artifacts and specs to stdout, where they land in CI logs.

`.env` is the conventional home for secrets and `env:` is exactly where they
would be interpolated, so this is a direct channel from a gitignored file
into world-readable artifacts and log output.

**Do not put a secret in `.env` for use in `env:`.** Give the daemon a
_path_ to a credential file it reads at runtime, and let the file system's
permissions protect the secret.

## An explicit `null` is not a way to unset a field

Writing `null` is an error wherever a manifest value is expected — as the
word itself, as `~`, as an empty value (`name:` with nothing after it), or
with an explicit tag (`!!null "null"`). Omit the key instead; that is what
selects the default. The one exception is `stop-action`'s `timeout`, where
`null` is a value: no timeout.

An empty _value_ and an empty _string_ are different things, and only the
first is a `null`. `name:` with nothing after it is YAML's null and is
rejected; `name: ""` is the empty string, which is a value the author wrote
and stays legal. The same distinction makes `env: {A: }` an error and
`env: {A: ""}` a normal assignment — `FOO=` is a normal thing to write.
Quoting is what tells the two apart, and it is the same rule that keeps a
quoted `"null"` a legal name, key, value and daemon id: it is a string the
author chose, not a YAML null. It is why `!!null ""` is the empty string
too — the tag says null, but the content is a quoted empty string, and the
content is what the value is.

The rejection exists because YAML's `null` was not reaching the manifest as
"absent". It arrived three different wrong ways, depending on the position:

- **A field that has a default** read `restart: null` as _unset_ and quietly
  applied the default. What the author explicitly wrote was discarded with
  no diagnostic.
- **A string that is not a field** — an `env` key or value, a `command`
  element, `user.name`, the daemon `id` — became the four-character text
  `null`. `env: {DEBUG: null}` installed a service whose `DEBUG` was the
  string `null`; `id: null` produced a daemon installed as `null.service`.
- **A container** — `env:` or `command:` with nothing under it — became an
  _empty_ container.

## An `env` key may not repeat

An `env` key that repeats, or that differs from another only in case, is
rejected rather than silently keeping one of the two. Case counts because
Windows looks an environment variable up case-insensitively, so
`{Path: a, PATH: b}` would reach the daemon as one variable.

## Paths in `command`

`command[0]` is resolved against the manifest's directory and written back
absolute, before any backend sees it.

`command[1..]` is not. `execve`, `posix_spawn` and `CreateProcess` all take
an argument vector as opaque bytes — no launcher on any platform resolves a
path _inside_ an argument. A relative path in `command[1..]` is therefore
resolved by the program being launched, against whatever working directory
that program sees at the time; goetia has no say in it.

`command[0]` is different because, unlike an argument, it is a field with a
defined role, and the three platforms disagree about that role for a
relative value. systemd requires "either an absolute path to an executable
or a simple file name without any slashes", resolving a bare name against a
fixed compile-time search path and **never** against `WorkingDirectory=` —
so a manifest's `bin/frpc` is rejected outright there. A Windows service has
no working directory of its own at all, and resolves a relative binary
against `System32`. launchd does neither: it resolves `ProgramArguments[0]`
against the job's working directory, which is emitted only when `cwd` is
set, so a relative binary there silently resolves against the default rather
than being refused. Absolutizing `command[0]` against the manifest's
directory collapses those three disagreeing rules into one that always
holds.

**The dragon is in the default.** Because only `command[0]` is absolutized,
a manifest that passes a relative path as an _argument_ —
`command: ["bin/frpc", "-c", "host/frpc.toml"]` — and does not set `cwd`
inherits whatever working directory the platform defaults to. systemd
documents that default, for system instances, as the **root directory**:
`-c host/frpc.toml` then resolves against `/`, and the daemon starts, cannot
find its own config, and exits with no indication that a working directory
was ever the problem. Set `cwd`, or write the argument absolute.

(`cwd` and `logs` are resolved against the manifest's directory too.)

## Directories: `install` makes them under system roots

`install` makes a `cwd` and the parent directory of `logs` if they are missing, but only under the places
the platform keeps a daemon's state and logs (the _roots_): `/var/log`, `/var/lib` and `/var/cache` on
Linux; `/var/log`, `/Library/Logs` and `/Library/Application Support` on macOS. A missing directory
anywhere else (`/opt/app/logs`, a home directory, a Homebrew prefix) is an error that names the path and
the roots: create it first. A directory that already exists is used as it is, wherever it is and whoever
owns it, since `install` is not a start and you may still be preparing it.

- **What it makes.** Every missing component, owned by root, `0755`. It makes a directory only where every
  directory on the way (resolved, so `..` and links count) is root's: a path through a directory an
  account owns is refused, which includes a directory an earlier install handed to its account (a second
  daemon's `cwd` below it). A symlink on the way is judged by the directory that holds it, not by its
  owner, since only whoever can write that directory can replace the link. The exception is a holder that
  is sticky and that a group or everyone can write (like `/tmp`), where the owner of an entry can delete
  it and make another: a symlink there that root does not own is refused. The path is judged as written,
  every directory it names included (one it leaves through `..` too), because the service manager
  resolves it by name when it starts.
- **Who gets what.** Only the deepest directory of a path is handed over, and only where the daemon's
  account has to write it: `cwd` goes to a non-root `user` (and its primary group) on both backends; the
  log directory goes to it on launchd, which opens the log as that user. On systemd the log directory
  stays root's: systemd opens `append:` as root before it drops privileges, and a directory an account can
  write lets it plant a symlink there and make root append to any file. A daemon that writes other files
  beside its log needs a `cwd` for them.
- **A directory appears only in its final state.** `install` makes the first missing component of a path
  beside where it will be, under a temporary name (`.goetia-` and a random one), and the rest inside it;
  it hands over what is to be handed over, and last renames the tree into place with a rename that
  replaces nothing. All of this is done before the write that can start the definition, so a run that
  follows finds its directories as they are to stay, and an install stopped at any point leaves no
  half-made directory at the path. A path that something else made after `install` looked fails the
  install and is left untouched, and running the install again uses it. Two installs running at once whose
  new paths share a missing parent may therefore fail one of them; a re-run succeeds (see
  [the concurrent-verb boundary](#the-boundary-that-is-guaranteed-and-the-two-that-are-not)). A file system
  that cannot rename a directory without replacing what is at the name (Linux's `RENAME_NOREPLACE`,
  macOS's `RENAME_EXCL`) fails it with an error that says so.
- **What is judged, and when.** The checks above judge the paths as they are when `install` runs. A change
  made during or after it, by hand or by another goetia command running at the same time, is not judged;
  see [the concurrent-verb boundary](#the-boundary-that-is-guaranteed-and-the-two-that-are-not).
- **The log directory of a job whose log root opens.** On systemd that is every daemon (it opens `append:`
  as root, before it drops privileges); on launchd it is a job whose `user` is root (uid 0, by any
  spelling), since launchd opens `StandardOutPath` as the job's user. A launchd job that runs as another
  user has its log opened as that user, and its log's directory is not judged. For such a log, `install`
  refuses a `logs` whose directory, or any directory on the way to it, a user other than root can change
  (owned by another user, or writable by everyone), and names who. Above the log's own directory, the
  group of a directory root owns is trusted, as systemd's own `LogsDirectory=` trusts it: Debian and Ubuntu
  ship `/var/log` as `root:syslog 0775`, and `/var/log/<app>/app.log` is accepted there; macOS ships
  `/Library/Logs` as `root:admin 0775`. goetia does not evaluate ACLs: a directory on the way that carries
  one, whatever it grants, is refused, with an error that names it. That is a POSIX ACL that is more than
  the mode, an NFSv4 or CIFS ACL, or a macOS extended ACL (`chmod +a`), on any directory of the path
  including the one a missing log directory is made in. A directory whose ACL lets nobody write is
  refused too (a macOS directory with a deny-only entry, a POSIX ACL whose mask has no write): move the
  log, or remove the ACL. The ACL is read from the directory the path resolved to, which needs no read
  permission on it; a read that fails refuses the log, with the reason (on Linux it goes through
  `/proc/self/fd`, so where `/proc` is not mounted the log is refused, and the error says so). An NFSv4 or CIFS file system
  always has one, so a log on it is refused. The log's own directory is judged strictly: a group that can
  write it is refused too, so `/var/log/app.log` is refused on Debian and Ubuntu, and the error suggests `/var/log/<app>/app.log` where
  `install` would make that directory (it is under the places above); anywhere else it says to use a
  directory only root can change, or to drop `logs:` (on systemd, to log to the journal). The sticky bit
  excuses a directory everyone can write above the log's, which it keeps others from renaming, but not the
  log's own directory, where it does not keep them from making an entry (a symlink at the log's name). A
  symlink on the way is judged by the directory that holds it, as for any path (above): where that
  directory is sticky and a group or everyone can write it, a symlink that root does not own is refused too,
  since its owner can replace it. A log that already exists is refused when it is a symlink or an account's
  file. A log in or under the `cwd` that is to be handed to the account is refused. All of this is judged
  before any directory is made and covers a directory that already exists, except that a log directory that
  is the `cwd`, or in it, is found when `install` reaches the `cwd` (on a file system that ignores case two
  names can be one directory), and what it made is then taken back.
- **A path that is not a directory** (a file, or a symlink that leads nowhere) is refused by both
  backends.
- **An account with no passwd entry.** On launchd a `user` with no passwd entry is an error. On systemd a
  `user` that is a number (a uid, or a name written `"1000"`: digits only, no sign, no leading zero) is that
  uid, as systemd reads `User=`, and is never looked up as the name of an account made of digits. `install`
  refuses 65535 and 4294967295 on every install, before it makes anything: systemd does not load a unit that
  names them. A `user` that is a name is looked up only when `install` makes a `cwd` to hand to it: one that
  does not exist yet, with no `cwd` or one that exists, installs as it did. A uid with no passwd entry is
  accepted like a name not created yet, and its `cwd` is handed to the number, with the group left as it was
  made. Until the account exists every start of the unit fails with `217/USER`. uid 0, by any spelling, is
  root and is handed nothing, on both backends.
- **A failed install** removes the directories it made, deepest first, by descriptor and only if they are
  still empty. It keeps them when the definition it wrote stands, and when it cannot tell whether it does
  (the unit or plist cannot be read back): it then says so and keeps them, since removing them could
  break a definition that stands. A directory that has gained content (the account's own
  writing, in one handed to it) is left, and named, and so are the directories above it.
- **An install stopped abruptly** (power loss, `SIGKILL`) may leave a `.goetia-*` directory beside the
  path, holding only the empty directories it was making. Nothing uses it, and `rm -r` removes it.
- **scm** makes none of these at `install`. The shim makes the missing parent of a user's own `logs:` path
  when the daemon starts.

## Per-backend overrides

One manifest describes a daemon on all three platforms, but a path, an
account or a service name is rarely the same on all three. A daemon's
`backend-specific:` block holds a set of field overrides per backend, keyed
by `systemd`, `launchd` or `scm`:

```yaml
id: frpc
command: [/opt/frpc/bin/frpc, -c, /opt/frpc/host/frpc.toml]
cwd: /opt/frpc
env:
  RUST_LOG: info
restart: on-failure
backend-specific:
  systemd:
    user: frpc
  launchd:
    user: frpc
  scm:
    command:
      ['C:\Program Files\frpc\frpc.exe', -c, 'C:\ProgramData\frpc\frpc.toml']
    cwd: 'C:\ProgramData\frpc'
    user: 'NT AUTHORITY\LocalService'
    env:
      RUST_LOG: debug
```

Every field but the daemon's id and `type` is overridable: `name`, `command`,
`cwd`, `env`, `user`, `restart`, `restart-delay`, `logs`, `protocol`,
`schedule`, `stop-action`. A scalar or list
field replaces the base value outright; `env` merges key by key, the
override winning where both set the same key, and `stop-action` merges
leaf by leaf. An unknown backend key is an
error naming it, as is a repeated one.

The two document-wide rules reach in here unchanged, and mean the same thing
they do in the base spec:
[an explicit `null`](#an-explicit-null-is-not-a-way-to-unset-a-field) is an
error anywhere in the block, and
[an `env` key may not repeat](#an-env-key-may-not-repeat) within one
override's `env`. A base key and an override of the same key are not a
repeat — that is the merge, and it is the point.

**Only the backend native to this host is installed, but every backend's
override is validated on every host.** So the manifest above renders on
Linux as the systemd-effective spec, with the `scm:` block validated and
then discarded:

```console
$ goetia daemon show -f goetia.yaml
# frpc
command:
- /opt/frpc/bin/frpc
- -c
- /opt/frpc/host/frpc.toml
cwd: /opt/frpc
env:
  RUST_LOG: info
id: frpc
name: frpc
protocol: plain
restart: on-failure
type: service
user: frpc
```

A value that is wrong for a backend by construction fails the whole manifest
from any host, naming the block it came from. Replace that `scm:` block's
account with a numeric uid — `user: {id: 1000}` — and the same Linux runner
rejects the Windows-only mistake:

```console
$ goetia daemon show -f goetia.yaml
error: daemon `frpc`: backend-specific.scm: `1000` is a numeric uid, which is never a Windows account
```

**`restart: always` is valid in the base spec, and refused when installing on
Windows.** SCM recovery actions fire only on a failure, never after a clean
exit, so a Windows service cannot be made to restart "always". Every host
resolves the manifest, so `show` works anywhere, for both `protocol: plain` and
`protocol: native`; the `scm` backend refuses it when it generates the service,
in `install` and `install --dry-run`:

```console
$ goetia daemon install --dry-run -f goetia.yaml
error: daemon `frpc`: `restart: always` cannot be honoured by the scm backend: SCM recovery actions never fire after a clean exit. To keep `always` on systemd and launchd, set `backend-specific: scm: { restart: on-failure }`
```

Override `restart: always` for Windows:

```yaml
restart: always
backend-specific:
  scm: { restart: on-failure }
```

A service installed by an earlier release with `restart: always` on Windows
is refused by `install` and `diff` as an unusable artifact, until you run
`goetia daemon uninstall <id>` and then `goetia daemon install`. `status`
fails for it and `list` reports it as `unusable`, because both render the
embedded copy of the spec and SCM now refuses it. `start`, `stop`, `restart`
and `uninstall` do not render it and keep working. What changes while it runs
depends on its `protocol`. A `protocol: plain` service is no longer restarted when
the command exits, because the new `goetia-shim` leaves every restart to SCM
and the service has no recovery actions. A `protocol: native` service is unchanged:
it already had on-failure recovery actions, keeps them, and was never restarted
after a clean exit. To give a `protocol: plain` service recovery actions, add the
override above to the manifest, run `goetia daemon uninstall <id>`, then
`goetia daemon install`.

`${VAR}` is substituted only in the override for the backend being
installed. The other two are validated as authored, never substituted, so a
check that would need a resolved value is skipped rather than guessed at:
from Linux, `scm: {user: "${ACCT}"}` is unresolvable and therefore
unchecked, while `systemd: {user: "${ACCT}"}` is substituted and checked
like any other native value.

A host warns only about its native backend, and reads the warning from the
substituted native spec, base fields included. Moving `protocol`, `restart` or
`restart-delay` into `.env` does not silence a warning or an error the
literal would have produced. systemd has no warnings. launchd warns about a
`restart-delay` that is not a whole number of seconds. scm warns about
`protocol: native` with `cwd` or `logs`, and about a `restart-delay` it must
clamp or round. A value written under a backend's own block is still
checked, and refused, from any host.

## Tasks

A task is a daemon that runs a program to completion and then waits for its
next run, which comes from its schedule or from you. It is declared with
`type: task`:

```yaml
id: backup
type: task
command: [/usr/local/bin/backup, --nightly]
schedule: "30 3 * * *"
logs: /var/log/backup/backup.log
```

A task takes `name`, `command`, `cwd`, `env`, `user`, `logs`, `schedule` and
[`stop-action`](#stop-action), which says how `stop` ends a run (a Windows task
has its own rule there); `restart`, `restart-delay` and `protocol` are refused, naming the
field (see [`type`, `protocol` and `schedule`](#type-protocol-and-schedule)). A task
without a `schedule` runs only when you ask, which is what a program that
triggers its own periodic work wants: it gets goetia's account and `env` set-up
and no timer.

What holds on every platform:

- **One run at a time, by design.** A schedule that fires while a run is in
  flight starts nothing, and so does a `start` over one (it succeeds). Parallel
  runs are not offered. On Windows two runs can still be alive at once, by two
  routes: for a task whose account logs on with a password, a start can be
  accepted before the previous run's process has exited (measured up to about
  20 ms; slower exits can widen it); and a run started outside goetia with
  `RunEx` ignoring constraints (`schtasks /run /I`) can overlap a goetia run for
  any account. `status` shows one pid, and `stop`, `kill` and `uninstall` act on
  every run alive.
- **Catch-up is always asked for**, with no field to turn it off; what is made
  up is the manager's own behaviour. systemd (`Persistent=yes`) runs a start
  the machine missed once it is up. launchd runs one missed while the Mac
  slept, not one missed while it was off (see
  [launchd](#launchd-calendar-entries)). On Windows, goetia sets
  `StartWhenAvailable`, Task Scheduler's setting for starting a task as soon as
  possible after a missed start. A run that came due while the task was
  disarmed is not made up on any platform: `enable` starts nothing (on Windows,
  nothing in the 12 minutes watched after it).
- **No retry.** A run that fails is not run again; a program that wants one
  retries itself. How it failed is its `last-result`.
- **Times are local.** See [the schedule](#the-schedule-per-platform).

### Ids are one namespace

A service and a task cannot share an id; on Windows `install` looks in both
SCM and Task Scheduler. `install` of a manifest whose `type` differs from the
installed daemon's removes the old daemon and creates the new one, and refuses
(exit `1`, saying to `stop` it and install again) while the old one runs. A
`start` made between that check and the removal is your own action, and
goetia does not guard it. On Windows a crash between the removal and the
creation leaves nothing at the id: install again.

A task's artifacts, by platform:

| Platform | A task is                                                                                                                                                             |
| -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| systemd  | `<id>.service` (`Type=oneshot`) and `<id>.timer`, both carrying goetia's marker.                                                                                      |
| launchd  | A job plist `<id>` with no calendar and no `RunAtLoad` or `KeepAlive`, and a timer plist `<id>@timer` that runs `launchctl kickstart` for it at the schedule's times. |
| Windows  | A Task Scheduler task `\Goetia\<id>` whose action is [the service host](#the-service-host-on-windows) in task mode.                                                   |

### The verbs

All under `goetia daemon`, as for a service:

- **`install`** writes the task disarmed: it will not run by itself until
  `enable`. An install over an installed task keeps it as armed or disarmed as
  it was (see [`install` never changes whether a daemon runs](#install-never-changes-whether-a-daemon-runs)).
  `install --enable` arms it, and is refused for a task with no schedule.
- **`enable`** arms the schedule, now and across reboots. systemd enables and
  starts `<id>.timer`; launchd moves both plists into `/Library/LaunchDaemons`
  and loads the timer; Windows writes the task's triggers back (the stored
  ones, or the ones the schedule compiles to). A task with no
  schedule has nothing to arm: `enable` is refused, naming the missing
  schedule, and its `enabled` is always `false`. On Windows, `enable` and
  `disable` of a task that runs as a real user need `GOETIA_SERVICE_PASSWORD`
  (see [Task accounts on Windows](#task-accounts-on-windows)). Arming makes up
  no run (see catch-up, above).
- **`disable`** disarms it. A run in flight carries on. On Windows it removes
  the task's triggers, which also stops a repetition in progress; a trigger
  added or changed by hand goes with them. Both `disable` and `enable` set the
  task's own Enabled flag true and register it under the account it is
  installed with. The triggers, that flag and the account are goetia's; a hand
  edit of anything else in the task survives.
- **`start`** requests a run now, armed or not, and returns once the manager
  has accepted the request. It says nothing of how the run went: `last-result`
  does. `--timeout` and `--no-timeout` are refused (see
  [Waiting for a start or stop](#waiting-for-a-start-or-stop)).
- **`stop`** and **`kill`** end the run in flight and leave the schedule armed
  ([Stopping and killing a task](#stopping-and-killing-a-task)). **`restart`**
  is a `stop` and then a `start`.
- **`run`** starts a run and follows it ([below](#run)).
- **`uninstall`** ends a run in flight and removes the task, or with `--no-stop`
  leaves the run alone ([Uninstalling a daemon](#uninstalling-a-daemon)). On Windows it also
  removes the task's run records, and never needs the account's password.

### `run`

`goetia daemon run <id>` starts one run of a task now, shows its output as the
program writes it, waits for it to end, and exits with its result. It needs
elevation, and `--json` is refused.

- A service is refused: `run` applies only to a task.
- A task with a run in flight is refused, as "already running", and no request
  is sent: `run` does not join a run someone else began. The state is read
  once, before the request, so a scheduled run that begins between the read and
  the request is one `run` may follow; it began after the command was asked.
  Two `run`s of one task at the same moment are your own action: each may
  follow the other's run, and goetia does not guard it.
- The output is the program's stdout and stderr merged, in the order it wrote
  them. It comes from the task's `logs` file or, on systemd with no `logs`, the
  journal. With neither, `run` prints one note that there is no output to show,
  and waits.
- The exit status is the program's own: its exit code, or `128` plus the signal
  that ended it (see [Exit codes](#exit-codes)). When goetia could not run the
  task it exits `1`, and when a run began, or may have, and goetia could not
  establish how it ended, it exits `4`. Repeating `run` after a `4` may repeat
  the run.

`run` finds its run in a different place on each platform. systemd: the system
journal, so `run` needs an account that can read it. launchd: goetia holds the
task's next launch with `launchctl debug`, starts it and watches the process.
Windows: the host records how each run ended (see
[the service host](#the-service-host-on-windows)), and `run` reads its run's
record.

On macOS, one case is not covered by `run`'s own checks:

> macOS: if `goetia daemon run` is killed with SIGKILL while it is starting the
> run, or while it waits for a scheduled run that started at the same moment, or
> if `goetia daemon run` fails after holding the next launch, or is interrupted
> while it waits for a scheduled run, that run or the task's next one may wait
> stopped, and no scheduled run starts while it waits; `status` shows it as
> running. `goetia daemon stop <id>` ends it, and later runs start normally.

A `run` that fails or is interrupted in that window says so in its error. A hold
that no launch has taken is consumed by the next `goetia daemon run <id>`.

A run that began, or may have, and that `run` cannot follow to its exit ends
with exit `4` and an error saying why. On systemd that is a journal that keeps
no service-manager messages (see [Status of a task](#status-of-a-task)); on
launchd, a launch that ended before goetia could follow it; on Windows, a run
that was killed, whose host failed, or that Windows' shutdown ended, none of
which leaves a record of the exit.

### Output

Where a task's output goes:

- **With `logs`**, the program's stdout and stderr, as for a service. The host
  or wrapper writes one line `goetia-run <identifier>` first in each run, which
  is how `run` finds where its own run starts: it shows the output up to the
  next such line and hides the lines themselves. A program that prints such a
  line itself is your own action.
- **systemd without `logs`**: the system journal, which is also what `journalctl -u <id>`
  shows. `run` shows it as the journal stores it. The journal stores no empty
  lines, and journald can store a line twice when it rotates a journal file
  during the run (measured on systemd 255). Set `logs` to keep the output
  exactly as the program wrote it. journald also drops lines beyond its
  per-unit rate limit, `RateLimitBurst` (scaled by free disk space) per
  `RateLimitIntervalSec` (30 s by default): a task that writes more loses lines,
  and a run started shortly after such a run may show none (`run` prints a
  note and the exact exit). Lifting the limit is host-wide: `RateLimitBurst=0`
  under `[Journal]` in a `journald.conf.d` drop-in, and a restart of journald.
- **launchd and Windows without `logs`**: nowhere goetia reads, and a Windows
  task writes its output to `NUL`. `run` waits, reports the exit, and says why
  it shows no output. This is deliberate: a task with no `logs` may have none
  for a reason, such as privacy.
- **Windows, the shim's own diagnostics** (a program that did not start, a stop
  that could not end everything) go to `%ProgramData%\Goetia\logs\<id>.log`
  and the Windows Event Log.

Two daemons that share one `logs` path are not refused. On systemd, a task's
output in that file ends at the next `goetia-run` line, which may be another
daemon's.

On systemd, a task starts through `/bin/sh`, which prints the marker and then
`exec`s the program. The program therefore also sees `PWD` (its working
directory) and, where `/bin/sh` is bash, `SHLVL=0`; an `env` that sets `IFS`,
`PWD`, `SHLVL`, `PS4`, `_` or `OLDPWD` is refused, because the shell resets
them. A command that is missing or not executable exits `127` or `126` from the
shell, not systemd's `203/EXEC`.

### Status of a task

`status` and `list` report a task with the fields of
[the `--json` document](#the-document).
`state` is `running` while a run is in flight and `idle` between runs; `pid` is
the run's process (on Windows, the task's shim host:
`%ProgramFiles%\Goetia\goetia-shim-<hex>.exe` for the `goetia` CLI and
`EmbeddedStockShim`, `goetia-shim.exe` where an installer places it for
`StandaloneShim`, the program itself where it calls `goetia::shim::main()`, or
whatever a custom `ShimProvider` supplies). `enabled` is whether the schedule is armed.
`last-result` is how the most recent finished
run ended, `last-run` when it started and `next-run` when the schedule starts
the next one. A task that has never run is `idle` with `last-result` and
`last-run` `null`, and `status` still exits `0`. Its `next-run` is `null` where
the schedule is not armed: the task is disabled or has no `schedule`, its
schedule never fires (`0 0 30 2 *`), or an `enable` or `install` was
interrupted before the schedule was armed again (launchd). On systemd it can
also be `null` while a run is in flight. Otherwise an enabled
task has its next run before it has had a first. A task's timer or triggers
changed by hand outside goetia may read otherwise.

The three differ by platform where a run is in flight, or the manager failed
a run before it exited:

|                                                                                         | systemd                                                                   | launchd                                                                                      | Windows                                                                        |
| --------------------------------------------------------------------------------------- | ------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ |
| Read from                                                                               | the system journal (`last-run`, `last-result`) and the timer (`next-run`) | launchd's last exit code (`last-result`); `next-run` is computed by goetia from the schedule | Task Scheduler (`LastTaskResult`, `LastRunTime`, `NextRunTime`)                |
| `last-result` while a run is in flight                                                  | `null`                                                                    | the previous run's result (`null` if none ever ended)                                        | `null`                                                                         |
| `last-run`                                                                              | the start of the newest run, so a run in flight's                         | always `null`                                                                                | the start of the newest run, so a run in flight's; `null` before the first run |
| A run the manager failed before the program exited (a `resources` or `timeout` failure) | `idle`, `last-run` set, `last-result` `null`                              | not told apart                                                                               | no such case                                                                   |

There is no field that carries the reason for a failed run. On systemd it is in
`run`'s error and in `journalctl -u <id>` (#287). On Windows, a start that fails
to log on the account (the account lacks the right to log on as a batch job, or
its stored password is stale) starts nothing and raises nothing, and leaves the
previous `last-result` and `last-run` as they were.

On launchd, `next-run` is computed in the zone of `/etc/localtime`, which is the
zone launchd schedules in, and printed in the process's zone, which honours
`TZ`. When `TZ` differs from `/etc/localtime`, its wall-clock reading differs
from the schedule's: `30 3 * * *` can read as another hour. The instant is the
same.

- **systemd:** `last-run` and `last-result` come from the system journal. An
  account that cannot read it (not root and not in `systemd-journal`, or
  `adm`/`wheel` where the distribution grants those) gets an error for the task
  instead of its status; a service's status needs no journal. On a host whose
  journal does not keep the service manager's messages (journald off,
  `Storage=none`, or `MaxLevelStore=` below `info`), `goetia daemon run` cannot
  follow the run: it ends with an error saying the outcome is unknown (exit
  `4`); the run itself may still happen. There, `status` shows state and next
  run but not the last run or its result; use `start`.
- **Windows:** `last-result` is the exit code Task Scheduler recorded for the
  host, which is the program's own unless the host ends the run itself: `1003`
  (it could not show that the program's tree ended), `1` (it refused to run: the
  id is not a goetia id, or the task has no goetia marker, is a service's, or has
  data that would not decode) or a Windows error number (it failed at something
  else; `31`, `ERROR_GEN_FAILURE`, when it has no number of its own: the task is
  gone, another install registered it again, or a removal marker stands). A program that exits with the same
  number reads the same. After a stop it is `0xC000013A` whatever happened: it is Task Scheduler's, and
  `run` reports the run's own record (see [Stopping and killing a task](#stopping-and-killing-a-task)).
  An unelevated `status` or `list` of a task that runs as another account (SYSTEM,
  LocalService, or an administrator's own elevated task) cannot open the run's
  process, so it cannot tell running from idle. It reports the id as `unusable`,
  naming the denial, exit `4`. An unelevated `run` never gets that far: it
  checks for elevation first and fails with the elevation error, exit `1`.
  Re-running as Administrator clears it.

### Stopping and killing a task

`stop` and `kill` end the run in flight, on every platform leave the schedule
armed, and leave the task `idle`. A `stop` of a task with no run is a success.
`stop --timeout` bounds the wait for the run to end, and its expiry is exit `4`
with the request standing.

- **systemd:** as for a service: `stop` stops the unit, and `kill` sends
  `SIGKILL` to its main process through a pidfd.
- **launchd:** `stop` sends `SIGTERM`, then `SIGKILL` once the `stop-action`
  `timeout` is over, and `SIGKILL` at once for `send-signal: kill`; it waits
  (bounded by `--timeout`), and what a short budget leaves running is told in
  [`stop-action`](#stop-action). `kill` sends `SIGKILL` at once: it reads the
  run's pid from launchd, holds that process, confirms with `launchctl procinfo`
  that it is the job's, and signals the held process. `stop` signals the held
  process too, never by label, and goetia boots the job out for neither. Only
  `run` signals by label: it sends `SIGCONT` to the launch that took its hold,
  its own or a scheduled one that took the hold of an earlier interrupted `run`,
  and follows that launch. A `stop` under any budget, `--timeout 0` included,
  first waits for a start launchd has accepted to begin, so that it can end it.
  A run that begins while a `stop` waits is ended too; under `--timeout 0`, or
  once a bounded budget is spent, a run that begins later is sent nothing.
  A `stop` or `restart` of a task that launchd shows in a state goetia does
  not know fails (exit `4`, naming the state) until goetia learns that state;
  `kill` and `uninstall` still work. `restart --timeout 0` of a task whose run
  is still alive is refused with exit `4`, saying that no start was issued:
  launchd starts no new run of a task while one is in flight, and `start`
  after it exits, or a `restart` without `--timeout 0`, does the job.
- **Windows:** a task's `stop` sends the console close event
  (`CTRL_CLOSE_EVENT`), as Task Scheduler does to any console program.
  Measured on Windows Server 2025 (build 26100): Windows allows one second for
  the whole run, counted from the stop. `run` reports how the program ended:
  `0xC000013A` when the event ended it, or its own code when it handled the
  event and exited. `status`'s last result is Task Scheduler's, and reads
  `0xC000013A` after any stop. A program still running when the second is over
  is ended by Windows together with the host, and that run leaves no record:
  `run` says so. A program with no console (a GUI program, or one that
  detached from it) is sent nothing: the host records the stop and waits for
  the program's own exit, which is then the run's record, and a program still
  running when the second is over is ended with the host and its run is
  recorded as stopped. A `Stopped` record means a stop was accepted and the host
  never saw the program exit, not that the program's tree has ended. A stop that
  arrives before the host has started the program starts nothing, and the run is
  still recorded as stopped. A stop that arrives in the moment between that
  check and the start still starts the program, which never receives the event
  and runs until it exits or Windows ends the host and its Job Object takes the
  program's tree with it. A run in flight at a system shutdown is ended by
  Windows and may leave no record.
  `kill` terminates the host, and its Job Object takes the program's tree with
  it; a waiting `run` then reports that the run ended without recording its exit
  (exit `4`), as it does for a shutdown or a host failure. When a stop could not
  end everything the program started, the command that stopped it says so, and
  so do the host's log and the Windows Event Log; `status` then shows the run as
  stopped (exit code `0xC000013A`), as a service shows stopped.
  `restart --timeout 0` is refused as on launchd.

### The schedule, per platform

A schedule is read in the machine's local time zone. `H` is resolved when the
task is installed, from the host name as the OS returns it (`gethostname()`), the
id and the field's position. A changed host name therefore shows as drift
(`diff` exits `3`), and the next `install` moves the run. On macOS `gethostname()`
returns `HostName` when it is set, and otherwise `LocalHostName` followed by
`.local`; `ComputerName` has no effect (measured on macOS 26). Whether a name
the network supplies replaces `LocalHostName` when `HostName` is unset was not
measured: if one does, an `H` schedule would read as drift after a network
change.

Platforms differ in what they can hold. One that cannot hold a schedule
refuses it at `install`, naming the limit, and nothing is written. A schedule
that names only dates that never exist (`0 0 30 2 *`) is accepted and never runs,
as in cron, and has no `next-run`.

#### systemd timers

- A time the clock skips (the clocks jump forward) is not run that day; a time
  that happens twice (they go back) is run once.
- Cron's either-or day rule needs two `OnCalendar=` lines, which goetia writes:
  systemd's own rule for a day and a weekday is both must match.
- A run starts within a second of its time.
- **A changed schedule on an armed task.** `install` disarms the timer, writes
  the new schedule, reloads and arms it again. A run that came due in that
  window is not made up; `goetia daemon start <id>` runs it.

#### launchd calendar entries

launchd takes `StartCalendarInterval` entries and reports no next run, so goetia
computes `next-run` from the entries it gave launchd.

- **Time zone.** Entries are computed in the zone launchd booted in. After a
  zone change launchd keeps the old zone until `UserEventAgent` restarts (measured
  on a CI runner), while `next-run` reads the new `/etc/localtime`, so they differ
  until then. Whether launchd recomputes after a zone change on a Mac with SIP on
  was not measured.
- **A time the clock skips or repeats** is read per entry. A schedule that names
  its hour runs once, at the end of the gap; an hourly or every-minute
  schedule skips the missing hour. A repeated time: an entry that names a date (a
  day, or a month and a day) runs at the first pass only, and one that names no
  date, or a weekday, runs at both. So `30 1 * * *` runs twice, and `30 1 15 * *`
  or `30 1 * 11 *` (which names each day of November) once. This was measured in
  `America/New_York`; a zone whose change is not one hour (Lord Howe's 30
  minutes) or falls at midnight was not.
- **launchd refuses** a schedule whose two day fields are both restricted and
  must both match (one starts with `*`, as in `0 0 */2 * 1`); any schedule that
  must name 29 February (`0 0 29 * *`, `0 0 29 2 *`, every day of February as
  in `0 3 * 2 *`, and every other day, `0 0 */2 * *`): launchd has no year, so
  it cannot run on 29 February only in leap years; and a weekday under a
  restricted month (`0 0 * 6 1`). A schedule whose day of the month and day of
  the week are either-or is held.
- **Missed runs.** macOS: a run due while the Mac is off or starting up, or while
  `goetia daemon install` replaces an enabled task's plists, is missed;
  `goetia daemon start <id>` runs it. A run due while the Mac sleeps runs when it
  wakes, which launchd does not let goetia turn off.
- **Timing.** On macOS a scheduled run starts up to a few seconds after its
  minute, and never less than a second after the previous run's start.
- **Many entries.** On macOS, a schedule with many entries (tens of thousands:
  every minute of restricted days or months) makes `enable` take longer, and
  launchd needs minutes after it returns to register every entry; a run due
  before then may be missed. `goetia daemon start <id>` runs it.
- **`last-run`** is always empty.
- A launchd task with `logs` starts through `/bin/sh`, as a systemd task does,
  and an `env` that sets `PWD`, `IFS`, `OLDPWD` or `_` is refused.

#### Task Scheduler triggers

Task Scheduler holds at most 48 triggers per task. goetia compiles a schedule to
triggers by a fixed rule: the minute `:00`, if the schedule names it, is a lone
trigger in each hour it names; the other minutes are grouped greedily, each
group the longest progression that starts at the smallest minute left, and a
group is one trigger per hour it names (a repetition inside the hour, or a lone
trigger for one minute). A schedule that compiles to more than 48 is refused:

```console
error: daemon `backup`: schedule `0-10,30 * * * *` compiles to 72 Task Scheduler triggers, and a task holds at most 48
```

`*/15 * * * *` and `* * * * *` are 48 each. The grouping is greedy and does not
always find the fewest triggers, so a schedule that a cleverer grouping could fit
in 48 can be refused. The way out is to split the schedule across two tasks.

- A schedule whose day of the month and day of the week are both restricted
  (cron's either-or, where a date both name would start the task from two
  triggers at one instant, which `IgnoreNew` does not reliably make one run) is
  refused, as is one where both must match, which Task Scheduler's day forms
  cannot hold. Use `*` for one of the two. The exception is an either-or
  schedule whose days of the month fall in no month of its month set: it is
  accepted, and only its weekdays count. `0 0 30 2 1` runs on Mondays in
  February, never on the 30th.
- **Clock changes.** A skipped hour runs nothing that day, and a repeated hour
  runs in both passes. A trigger due at the instant of the change does not run
  (the first minute after a skipped hour, and the start of the second pass of a
  repeated hour), so the `:00` run is the one a change at the hour costs. No
  repetition crosses a local hour boundary, which is why every hour has its own
  trigger. In spring, a task with an every-minute repetition (`* * * * *` or
  any step-1 group) registered shortly before the change ran once a minute from
  registration until the repetition's first start, at minutes the schedule does
  not name (registered 8 minutes ahead, it ran 8 extra times); how that depends
  on the time of registration is open ([#285](https://github.com/bindreams/goetia/issues/285)).
  A change off the hour, at midnight or by other than one hour (Lord Howe's 30
  minutes) was not measured (#285), and the above is not claimed for one.
- **A changed schedule** to a lone time starts no run: a lone trigger moved to
  a time between the last run and now started nothing in the 12 minutes
  watched.

### Task accounts on Windows

A task runs as SYSTEM, LocalService or NetworkService, or as a real user. A real
user's task logs on with a password read from `GOETIA_SERVICE_PASSWORD`, as a
service's does, and `install` grants the account `SeBatchLogonRight`; a virtual
account (`NT SERVICE\...`) is refused for a task (Task Scheduler cannot run one).
`enable` and `disable` of such a task re-register it and need the password;
`uninstall` never does, so a task whose account was deleted or whose password
was lost can still be removed. A failed logon (no right, or a stale password)
starts nothing and raises nothing.

- A `%` in the path or arguments of the host (`goetia-shim`) is refused for a
  task, because Task Scheduler expands `%VAR%` in a task's arguments. The task's
  own `command` is not affected.
- An account change registers the task again, so its last run and last result
  start over. It is refused while a run of the task is alive (run
  `goetia daemon stop <id>`, then install again) and while something else holds
  its run records, such as a run that is starting or a `run` or `stop` of it
  (wait for that to end, then install again). A scheduled run that comes due
  while the change, or an `uninstall`, is under way does not run, even when the
  change is then refused. If the change fails after the old task was removed,
  nothing is installed at the id: install again, and `enable` it if it was
  armed. A wrong `GOETIA_SERVICE_PASSWORD` fails that way: Task Scheduler
  checks it only when it registers the new task.
- A task whose user was deleted and created again keeps the old account's SID,
  so `install` shows it as a conflict naming that SID; `install --force` (with
  the password) registers it for the new account. A task whose user was deleted
  or renamed shows the same conflict, and `install --force` then works only
  with a `user:` that exists (moving the task to another account takes
  `--force` too). `enable` and `disable` refuse until then. While the account
  cannot be looked up (an unreachable domain), the check is skipped.
- An install or an `uninstall` that did not finish can leave the task switched
  off and its run records closed: nothing starts it, and `start`, `run`,
  `enable`, `disable`, and a `stop` that finds a run to wait for fail saying
  so. Run the install again with `--force` (without it the switched-off task
  reads as a hand edit), or run `uninstall` again. A run that outlived a failed
  `uninstall` is not ended by a `stop` that waits, which is refused;
  `stop --timeout 0`, `kill` or the next `uninstall` ends it.
- A service `uninstall` while Task Scheduler is unusable cannot tell whether a
  task still references the host's file, so it leaves the file and reports it;
  the next sweep removes it.
- A directory someone put by hand in a task's records directory (only SYSTEM or an
  Administrator can) that cannot be removed fails `uninstall`, naming it, after
  the task is deleted: remove the directory by hand and run `uninstall` again,
  or install the task again.
- A task whose run-records directory is missing or unusable starts no program;
  the Event Log names the path. `install` of the task, an up-to-date one
  included, makes a missing directory again.
- A task that runs as a local account that has never logged on may never start on
  that first start: Windows' logon of the account waits in the User Profile
  Service, `Run` returns, no process starts, and `schtasks /query` of the task
  does not return. Measured on Windows Server 2025, in about one of ten first
  logons made back to back. Whether goetia's own `status` and `list` block on it,
  whether it recovers without a reboot, whether an account that has logged on once
  is affected, and whether a service's first start is ([#275](https://github.com/bindreams/goetia/issues/275))
  were not measured.

## Machine-readable output

`--json` is implemented by `goetia daemon list` and `goetia daemon status`.
On any other subcommand it is **refused** — with the same envelope, a single
`unsupported` error, and exit `2` — **before that subcommand runs**, so
nothing is installed, started, or removed by a command whose output you
could not parse.

**The invariant:** whenever `--json` is given together with a subcommand
that clap accepted, stdout is exactly one JSON document, and nothing else.
Warnings and human-readable errors go to stderr.

The carve-out is what clap short-circuits before goetia's dispatcher runs:
`--json --help` and `--json --version` print their own text and exit `0`,
and a command line clap rejects (`goetia --json daemon uninstall`, with no
ids) prints clap's message to stderr, leaves stdout **empty**, and exits
`2`.

### The document

Goetia writes one compact object on a single line, newline-terminated. It is
shown pretty-printed here for readability:

```json
{
  "daemons": [
    {
      "id": "frpc",
      "type": "service",
      "state": "running",
      "enabled": true,
      "pid": 1234,
      "last-result": null,
      "last-run": null,
      "next-run": null
    },
    {
      "id": "backup",
      "type": "task",
      "state": "idle",
      "enabled": true,
      "pid": null,
      "last-result": { "code": 0 },
      "last-run": "2026-11-18T03:30:00+02:00",
      "next-run": "2026-11-19T03:30:00+02:00"
    }
  ],
  "errors": [],
  "undetermined": []
}
```

`daemons`, `errors` and `undetermined` are **always present**, as arrays,
possibly empty.

| Key                     | Type            | Notes                                                                                                                                                                                                                                                                                                                                                            |
| ----------------------- | --------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `daemons[].id`          | string          | The daemon id.                                                                                                                                                                                                                                                                                                                                                   |
| `daemons[].type`        | string          | `service` or `task`.                                                                                                                                                                                                                                                                                                                                             |
| `daemons[].state`       | string          | One of `running`, `idle`, `stopped`, `failed`, `unknown`. A task is `running` or `idle`, or `unknown` where its manager cannot say, and never `stopped` or `failed`; a service is never `idle`.                                                                                                                                                                  |
| `daemons[].enabled`     | bool            | Whether the service is enabled at boot; for a task, whether its schedule is armed (`false` for a task with no schedule).                                                                                                                                                                                                                                         |
| `daemons[].pid`         | integer or null | `null` means the manager reports **no main process** — never "goetia could not find out", which is an `errors` entry instead.                                                                                                                                                                                                                                    |
| `daemons[].last-result` | object or null  | How a task's most recent finished run ended: `{"code": n}` or `{"signal": n}`, where `n` is the signed 32-bit value (a Windows run that was stopped reads `{"code": -1073741510}`, which is `0xC000013A`). `null` on a service, and on a task that has not run or whose run is in flight, where the platform says so: see [Status of a task](#status-of-a-task). |
| `daemons[].last-run`    | string or null  | When a task's most recent run started, as RFC 3339 with the offset of the system time zone at that instant, to the second: `2026-11-18T03:30:00+02:00`. `null` on a service, on a task that has not run, and always on launchd.                                                                                                                                  |
| `daemons[].next-run`    | string or null  | When a task's schedule will start it next, in the same form. `null` on a service, on a task whose schedule is not armed, on one with no run to come, and, on systemd, possibly while a run is in flight.                                                                                                                                                         |
| `errors[].id`           | string or null  | The daemon id or service name the failure is attributable to; `null` for `unsupported`, and for an `unavailable` that belongs to no id.                                                                                                                                                                                                                          |
| `errors[].kind`         | string          | See below.                                                                                                                                                                                                                                                                                                                                                       |
| `errors[].message`      | string          | Human-readable detail.                                                                                                                                                                                                                                                                                                                                           |
| `undetermined[].name`   | string or null  | The id the entry stands for. **`null` is one entry standing for many ids** — see below.                                                                                                                                                                                                                                                                          |
| `undetermined[].reason` | string          | What did not complete, as facts: the operation, what it was on, and the failure. An aggregate's stands alone and names a count rather than an id. Whether it also carries a remedy is platform-specific — see [Which reads count, per platform](#which-reads-count-per-platform).                                                                                |

A daemon's display `name` is deliberately absent: `status` has no spec to
read it from, and one field differing between the two subcommands would be
worse than sending you to `goetia daemon show`.

`last-result`, `last-run` and `next-run` are keys of every entry: a service has
them as `null`, never absent, so a consumer reads one shape. A time is in the
system time zone as the process sees it, which honours the `TZ` variable, so
the same task prints a different offset under a different `TZ`; the instant it
names is the same. Where the system zone cannot be determined, a time is
written in UTC with `Z`. A time outside the years 0 to 9999, which RFC 3339
can write, is `null`. The text form of `status` prints a task's same fields,
with `-` for a value that is `null`; a `list` row ends with the daemon's type.

`errors[].kind` is one of eight values:

| `kind`          | Exit code | Meaning                                                                                                                                                                                                                                                                                                                                                                                       |
| --------------- | --------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `not-installed` | 1         | Nothing is installed at that id. Only from `status <id>`.                                                                                                                                                                                                                                                                                                                                     |
| `foreign`       | 1         | A read that _completed_ established that what is there is not goetia's: an artifact carrying no marker, or a masked unit's symlink. Never inferred from a read that failed. Only from `status <id>`.                                                                                                                                                                                          |
| `unusable`      | 4         | Goetia read enough to know the id is its own, but cannot report on it. `message` says why.                                                                                                                                                                                                                                                                                                    |
| `undetermined`  | 4         | Goetia could not determine **whether** anything is installed at that id: a read it needed failed. Claims no ownership — that is the whole difference from `unusable`. `message` names what would not read — a path, or on Windows a registry key or service object — and what would make it readable. Only from `status <id>`; out of `list()` the same fact is the `undetermined` key below. |
| `invalid-id`    | 1         | A command-line argument was not a valid daemon id. Fix the argument.                                                                                                                                                                                                                                                                                                                          |
| `unavailable`   | 1         | Obtaining the manager, or listing, failed, so **no** answer was obtained for any daemon — `id` is `null`. Or, under a daemon's `id`, goetia would not ask systemd about that daemon: offline, from a chroot, or where systemd did not boot.                                                                                                                                                   |
| `unsupported`   | 2         | `--json` was given to a subcommand that does not implement it.                                                                                                                                                                                                                                                                                                                                |
| `other`         | 1         | Unreachable today; reserved so an unclassified failure has a home rather than being silently dropped.                                                                                                                                                                                                                                                                                         |

### `undetermined`

`undetermined` carries what `list()` could not classify: an id whose read
never completed, so goetia established neither that something is installed
there nor that nothing is. A third key rather than an `errors[].kind`
because the command did not fail — it reported everything it could see, and
this is a stated limit on that answer's completeness, which a consumer must
be able to read without parsing `kind` strings.

**While an entry may stand for your id, "absent from `daemons`" does not mean
"not installed".** An entry whose `name` is `null` is **one entry standing
for many ids**, so while one is present you may not conclude that any
particular id is absent — not for rendering, not for a branch, not for a
test assertion. Standing for an unnamed set is the whole reason that entry
exists instead of hundreds of named ones. A _named_ entry is narrower: it
separates the id it names and blocks the conclusion for that id alone.

So `undetermined` being empty is not what you check. Reading a missing id as
uninstalled needs three things: no `undetermined` entry with a `null` name,
no `undetermined` entry naming that id, and no `errors` entry whose `id` is
`null`. The last is the listing that failed outright — `unavailable` yields
an empty `undetermined` alongside one error attributable to no id, and an id
is missing from that document because nothing was enumerated at all. A
_named_ `errors` entry, like a named `undetermined` one, blocks only its own
id. Two further conditions defeat even all three — a concurrent goetia verb,
and an id systemd's scan can name from nothing but a
`multi-user.target.wants` link;
see [what the guarantee covers](#the-boundary-that-is-guaranteed-and-the-two-that-are-not).

Named entries come first, sorted by name; aggregates last.

Only the `list()`-derived forms ever populate it: `goetia daemon list`, and
`goetia daemon status` with no ids. `status <id>` asks about one named id,
so the same fact arrives as that id's own `errors[].kind: "undetermined"`
instead. Both exit `4`, so a consumer branching on the exit code sees no
difference between them — the split is about attribution, not severity.

### Exit code

The exit code is **zero exactly when both `errors` and `undetermined` are
empty**, and otherwise the precedence-max of the codes in the table above,
an `undetermined` entry contributing the same `4` the `undetermined` kind
does (see [Exit codes](#exit-codes) for the precedence rule).

The one thing that overrides that: if stdout refuses the write — a broken
pipe (`goetia --json daemon list | head -1`), a full disk — the document was
not delivered, so the exit code is `1` and the reason goes to stderr,
whatever the report's own code would have been. Exit `0` never accompanies an
empty or truncated stdout, which is what makes "parse stdout first" safe.

Note `invalid-id` is `1`, not `2`. `2` means the parser rejected the command
line and nothing ran, but `goetia daemon status good bad` queries and prints
`good` before rejecting `bad` — claiming `2` for a partially executed run
would mislead exactly the consumer the code exists for.

So the exit status is not a "did I get JSON" test. A `4` still carries a
complete, well-formed document describing what could and could not be
determined: parse stdout first, then read `errors` and `undetermined`.

## `install` never changes whether a daemon runs

Installing over a running daemon neither stops nor restarts it, on every
platform, with one exception on launchd (below). What differs is when a changed
spec takes effect:

| Platform | Installing a changed spec over a running daemon                                                  |
| -------- | ------------------------------------------------------------------------------------------------ |
| systemd  | The daemon keeps running. Its next start of any kind, goetia's or `Restart=`'s or boot, uses it. |
| SCM      | The same: the daemon keeps running, and its next start of any kind uses the new spec.            |
| launchd  | Refused: `install` writes nothing, exits `1` and names the daemon. See below.                    |

launchd keeps the definition a job was loaded with and cannot update it, so
a rewritten plist would leave `start` running the old definition and reporting
success. `install` refuses instead, while launchd has the job loaded. That
includes a job that has exited, or crashed and is awaiting its relaunch:
"loaded" means launchd holds the job, not that it has a process. The remedy
is in the message:

```sh
goetia daemon stop <id>   # boots the job out of launchd
goetia daemon install     # now writes the new plist
goetia daemon start <id>
```

What is compared is the plist, not the spec: the one `install` would write
against the one on disk, each without goetia's own metadata comment (the
embedded spec and goetia version). `install` refuses when they differ. That
covers a changed spec, a `--force` overwrite of a hand-edited plist, a newer
goetia that renders the same spec differently, and an unchanged
`user: {id: ...}` whose account name has changed. It does not cover a spec
change the plist does not render, or a newer goetia that renders the same
text: those are written as usual and leave the loaded job as it is, since
launchd reads the same thing either way. `install --start` is refused the
same way, before it starts anything. `install --dry-run` and `diff` read no
job state and do not report the refusal. If something else loads the job
between that check and the write, `install` checks again after writing, puts
the plist back as it was and exits `4`: goetia cannot tell which plist the
loaded job read. So it does when that second check cannot read launchd's
answer. With the plist back, re-running `install` is refused while the job is
loaded, and the message gives the same stop, install, start remedy. The
put-back writes only over the plist `install` wrote: if a concurrent `enable`,
`uninstall` or install has moved, removed or replaced it since, it is left as
it is, and the message says what is on disk. When `launchctl print` fails for
any reason other than "no such job", `install` and `start` fail rather than
take the job to be unloaded.

**The exception.** Creating a daemon whose id has no goetia plist, while
launchd has a job loaded under the same label (a predecessor installed by hand,
say), stops that job: `start` would otherwise see the label loaded, take it
for the new daemon, and report success while the daemon never ran. The job is
booted out only after the plist is staged and checked, and the plist is created
after that, so a failure at any step leaves no plist and a rerun tries again. If
the job is loaded again once the plist is created, `install` removes the plist
and exits `4`. If another writer took the plist's place after the boot out,
`install` exits `4`, says the job was booted out and what is there, and leaves
it. If the plist is taken or removed after `install` has read launchd's state
but before it has sent anything, `install` exits `1`: no plist was written and
nothing was sent, and a rerun starts afresh.

**A task.** For a task, running means a run is in flight. `install` never ends
one, and never arms or disarms the schedule: a first install writes the task
disarmed, and an install over an installed task leaves it as armed or disarmed
as it was.

- systemd and SCM: a changed task takes effect at its next run, and the one in
  flight is not touched.
- launchd updates an idle task, which launchd holds loaded without a process:
  it boots the timer out where the timer renders differently, the schedule is
  dropped, the job is booted out, or the timer is stray, writes the plists, and, if the task was
  enabled, loads the job and then the timer again, so the schedule stays armed.
  It refuses only when the job's rendering, apart from goetia's metadata
  comment, changes, the job is loaded and a run is in flight (exit `1`, naming
  the task and saying to `stop` it, which leaves it idle), with nothing
  written. A schedule edit changes the job plist's metadata comment but not its
  rendering, so it is applied while a run is in flight and does not touch the
  run. A run due while
  the plists are replaced is missed (see [launchd](#launchd-calendar-entries)).
- Windows: an install that changes the account a task runs as registers the
  task again; see [Task accounts on Windows](#task-accounts-on-windows).

Installing a manifest of the other type over an installed daemon removes the old
one first (see [Tasks](#ids-are-one-namespace)).

## The service host on Windows

On Windows a `protocol: plain` daemon runs under a service host, goetia's shim
(`goetia-shim`), which hands itself to SCM and runs your command. A
`protocol: native` daemon speaks to SCM itself and never touches the shim.

`install` places the shim at `%ProgramFiles%\Goetia\goetia-shim-<sha256>.exe`,
named by the SHA-256 of its contents, and registers that path. The file is
created with a protected DACL: SYSTEM and Administrators have full control,
Everyone read and execute. `install` refuses, before writing anything, if
an account other than SYSTEM, Administrators or TrustedInstaller owns the folder
or any ancestor, or can change its permissions or owner, rename it, or delete
files in it. Every `protocol: plain` install verifies the file against its
name and repairs it, also when the service is already up to date. Goetia
versions on one machine each register their own file, and none deletes a file
another service still uses.

Where the bytes come from depends on the build:

- The `goetia.exe` of a release embeds the shim, and the archive ships
  `goetia.exe` alone: a lone copy installs `protocol: plain` daemons.
- `cargo install goetia` (an MSVC toolchain; other Windows targets fail to link)
  installs `goetia.exe` and `goetia-shim.exe`, and
  `goetia.exe` reads the one beside it. If that file is missing, `install` and
  `install --dry-run` fail naming its path.

`stop` of any Windows daemon ends the service's process once SCM reports
`STOPPED`, which it does first, and waits, within its `--timeout`, for that
process to exit. A `stop` and then an `uninstall` therefore finds the shim file
free. The one gap: if the process crashes and Windows' recovery restarts the
service before the stop request is sent, the request reaches the new process,
which goetia does not hold, so `stop` can return while it is still exiting, and
an `uninstall` right after may leave the shim file for the next sweep.

`uninstall` of a `protocol: plain` daemon stops and deletes the service, ends the
shim process it took hold of before the service stopped, waits, with no time limit,
for it to exit, and deletes every `goetia-shim-*.exe`
that no remaining service references, and the default folder when it is then
empty. If a file cannot be deleted, `uninstall` exits non-zero naming each file that
remains: the service is already deleted, and re-running
`goetia daemon uninstall <id>` sweeps the program's own shim folder again. That
also holds for an id that is not installed: it sweeps, then reports "nothing to
do" (exit `0`) when the sweep succeeded. Besides the provider's folder,
`uninstall` sweeps only `%ProgramFiles%\Goetia`, where a service installed by
another provider may have its shim. A file in any other folder is never swept:
a program's installer may place a file with a shim's name in its own folder.
`uninstall` waits only for a shim process it took hold of. When the service was
already stopped, or stopped before `uninstall` could take hold of its process (a
daemon that fails at start), a shim still exiting keeps its file in use; that file
is skipped without an error, and a later sweep removes it.

A [task](#tasks) runs under the same host, as the action of a Task Scheduler
task in the folder `\Goetia`, and the host is placed, verified and swept as above.
Task Scheduler starts it with `--goetia-task <id> <token>` after the host's own
arguments, which is task mode (the token is the task's registration token, which
the host checks against Task Scheduler's own record of the task): the host reads its own task, applies the task's
`env`, `cwd` and `logs`, runs the program in a Job Object as for a service, and
does not talk to SCM. It writes one record of how each run ended
to `%ProgramData%\Goetia\runs\<id>`, where `run` and `stop` read it (`status`
reads Task Scheduler, not these), and a later run prunes the older records.
`uninstall` reads them while it stops the runs, then removes them. An elevated
`install` makes `%ProgramData%\Goetia`, `Goetia\logs` and `Goetia\runs` with the
shim folder's permission check, and refuses one an account other than SYSTEM,
Administrators or TrustedInstaller can change, never repairing it: a
`%ProgramData%\Goetia` that an earlier goetia left to a non-administrator is
deleted, or given to Administrators, by hand.

### Shipping the shim from a library

A program that links goetia and installs `protocol: plain` daemons through
`goetia::backend::scm::manager::Scm` supplies the host itself, through a
provider given to `Scm::with_shim`. `Scm::new()` has none: installing a
`protocol: plain` daemon through it fails with "no shim provider configured",
while `status`, `list`, `protocol: native` installs and `uninstall` work.
`CheckPermissions::Disable` turns off only the permission check, of the host's
folder and of the host file; the marker check, which requires the file to
contain goetia's host code of the same version as the library, and the file
verification always run. The check is the manager's alone: a sweep checks the
folder it deletes in with it too.

Any program that ships a stock shim adds one file, `src/bin/goetia-shim.rs`:

```rust
goetia::stock_shim!();
```

and a `build.rs` that hardens it, as goetia hardens its own binaries: its imports
and its `LoadLibrary` calls without flags search System32 only, not the shim's
folder (Windows MSVC; on other Windows targets and Cygwin the package's bins, and
so its integration tests and benches, fail to link, while the library still builds):

```toml
[build-dependencies]
libsearch-guard = "0.2"
```

```rust
// build.rs
fn main() {
    // Without this, cargo reruns the script after a change to any file in the package.
    println!("cargo:rerun-if-changed=build.rs");
    libsearch_guard::bins();
}
```

That covers every `[[bin]]` of the package, the program itself included: once
hardened, a DLL it ships beside itself must be loaded with explicit flags, and its
VC runtime is linked statically. The
[libsearch-guard README](https://github.com/bindreams/libsearch-guard#readme)
lists what runs before the guard and how to load a sidecar DLL. The program then
chooses how the shim reaches the machine.

**Installer places the file.** The installer puts `goetia-shim.exe` in the
product folder, and the program points goetia at it:

```rust
Scm::with_shim(
    StandaloneShim::new(product_dir.join("goetia-shim.exe"), vec![]),
    CheckPermissions::Enable,
)
```

**A single executable.** The program embeds the shim and goetia places it under
Program Files (`EmbeddedStockShim::new(bytes, Some(dir))` picks another
folder). In `Cargo.toml`, declare the feature:

```toml
[features]
goetia-shim = []
```

then build in two stages: the shim, then the program with the feature on and
`GOETIA_SHIM_EXE` the **absolute** path of the built shim (`include_bytes!`
resolves a relative one against the source file). Stage 2 builds the program
target alone:

```sh
cargo build --release --bin goetia-shim
GOETIA_SHIM_EXE=/abs/path/target/release/goetia-shim.exe \
    cargo build --release --bin app --features goetia-shim
```

```rust
Scm::with_shim(
    EmbeddedStockShim::new(goetia::embed_shim!(), None),
    CheckPermissions::Enable,
)
```

Expand `embed_shim!()` in the binary crate, never in a library the shim binary
links. Without the feature the program still builds and type-checks with no
variable set, and installing a `protocol: plain` daemon fails with "built without
a stock shim". With the feature on and the variable unset, the build fails.

**The program is the service.** The program's own `main` hands over to the shim
when it is started as the host:

```rust
fn main() {
    // First, before anything else, whether or not this run is the host.
    goetia::shim::init_host();
    if std::env::args().nth(1).as_deref() == Some("--service-host") {
        goetia::shim::main()
    }
    // ...
}

Scm::with_shim(
    StandaloneShim::new(std::env::current_exe()?, vec!["--service-host".into()]),
    CheckPermissions::Enable,
)
```

`goetia::shim::init_host()` first in `main` is what lets a stop ask a daemon to exit on Windows (see
[`stop-action`](#stop-action)): the console event is sent by a copy of the host program that cosca starts, which
runs `main` from the top, and `init_host` is what turns that copy into the sender and ends it. Anything `main`
does before the call runs in every copy too, so it must not block or wait for something the running host
holds. A host that did not call it is refused when a service whose stop
asks starts. The stock shim (`goetia::stock_shim!()`) makes the call itself.

Tasks run under the same host, which Task Scheduler starts with `--goetia-task
<id> <token>` after the host's own arguments. A program that calls
`goetia::shim::main()` from its own `main` must let those three trailing
arguments through its own argument parser, and leave them last. The last is a
registration token that the host must pass on unchanged.

Any other source of the shim implements `goetia::shim::ShimProvider`.

## Waiting for a start or stop

`daemon start`, `daemon stop`, `daemon restart` and `daemon install --start`
share one pair of flags:

| Flag                   | Meaning                                                                                                                               |
| ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| `--timeout <DURATION>` | Wait up to `DURATION`. The default is `10s`; for `stop` and `restart`, the daemon's [`stop-action`](#stop-action) timeout plus `10s`. |
| `--timeout 0`          | Issue the request and return without waiting.                                                                                         |
| `--no-timeout`         | Wait indefinitely.                                                                                                                    |

On macOS, launchd keeps a job's label until the job's process has exited, and
goetia does not load a job over one that is still going away. So on macOS, for a
service (a task's `stop` never boots its job out: see [Stopping and killing a
task](#stopping-and-killing-a-task)):

- `daemon stop`, the stop leg of `daemon restart`, and `daemon uninstall` boot
  the job out and then wait until launchd no longer has it. `stop` and
  `restart` stop waiting when `--timeout` runs out; under `--timeout 0` or
  `--no-timeout`, and for `uninstall`, the wait has no bound.
- `daemon install` of a new daemon whose label a loaded job already holds boots
  that job out and waits, without bound, until it is gone.
- A start (`daemon start`, `restart`, `install --start`) that finds the job
  still going away waits, without bound, until it is gone, then loads it. This
  wait counts against `--timeout`.
- For `restart: always`, a start that does not find the daemon running boots
  it out, waits without bound until it is gone, and loads it again, except
  under `--timeout 0`.

These waits normally end within the job's `ExitTimeOut`, after which launchd
kills it. A wait with no bound can last forever if the job's process never
exits, for example when its plist sets `ExitTimeOut` to 0 and it ignores
SIGTERM.

The two flags are mutually exclusive. `install` accepts them only alongside
`--start`: without it nothing is started, so there is nothing to wait for,
and the command line is refused at exit `2` having run nothing. Under
`--dry-run` they are inert, exactly as `--start` itself is.

**A task.** A task's `start` requests a run and returns once the manager has
accepted it, so the two flags are refused for it, per id, at exit `1`; so are
they on `install --start` of a task. What `start` returns says nothing of how the
run went: `last-result` does. A task's `stop` and `restart` take both flags, and
the budget bounds the wait for the run to end.

**Durations** use the manifest's own grammar, the one `restart-delay`
accepts: `10s`, `500ms`, `2m30s`. Write a compound duration **unspaced**, or
quote it as a single argument. All four verbs take a variadic id list, so
`--timeout 2m 30s` passes `2m` to the flag and leaves `30s` to be read as a
daemon id — and goetia then reports that a daemon called `30s` is not
installed, which is a confusing answer to a typo the shell made.

### Restarts on Windows

Windows restarts a daemon through the service manager's recovery actions, for
`protocol: plain` and `protocol: native` alike. `goetia-shim` runs your command once,
reports how it ended as the service's exit, and exits. With `restart: on-failure`
SCM then starts a new shim after `restart-delay` (1s by default), and that shim
reads the spec as installed, so a daemon restarted after `goetia daemon install`
runs the updated spec. A shim that is killed or crashes once it has connected to
SCM is restarted too; one that dies before it connects (an early crash, or a
dispatcher connect that fails) is not, and `start` reports the failure. A
clean exit is never restarted, and a stop reports a clean exit. SCM stores
`restart-delay` in whole milliseconds, so a finer delay (`1500us`) is rounded up
(`2ms`), with a warning.

Nothing cancels a restart SCM has queued, and `goetia daemon stop` does not
look for one. A command that failed and is waiting out `restart-delay` finds
the service already stopped, so `stop` exits `0` and the daemon is started when
the delay ends. When a command fails just as a `stop` arrives, the shim decides
once: a stop it accepts before it decides the exit is reported as a clean stop,
which SCM does not restart; a stop that arrives after it has decided on the
failure does not change it. SCM acknowledges that stop anyway, whatever the
shim answers, so `stop` exits `0` and SCM restarts the command as above.
`goetia daemon uninstall` first sets the service's start type to disabled, so a
restart SCM had queued before the disable, or queues after it, fails and starts
nothing. A disable that lands after SCM has begun a restart attempt does not
stop it: the attempt runs to a running service, even when the disable follows
the failure by a fraction of a millisecond. `uninstall` then stops whatever is
running, such an attempt included, waits for it to stop, and deletes the
service. A stop that SCM rejects because the state changed is sent again on the
next state. If the stop or the delete fails, the service is left disabled and
not deleted; the start type is not put back. An interrupted `uninstall` (Ctrl-C,
a killed `goetia`) leaves the service as it was before the disable, disabled and
not deleted, or deleted, depending on where it stopped. In every case running
`goetia daemon uninstall` again is the retry. The first failure's error names
the start type the service had, when it could be read, and
`goetia daemon enable` or `goetia daemon disable` sets it again. On a retry the
service already reads as disabled, so a second failure says the original start
type is unknown. `goetia daemon install` over a service that a failed
`uninstall` left disabled keeps it disabled, so `start` fails with 1058 until
`enable` or `disable` sets a start type. A service that does not accept a stop
needs `goetia daemon kill <id>` first; `sc.exe stop` fails the same way. A
service someone else already marked for deletion is not disabled; if it will not
stop, the error says so, and it is removed once it stops and its last handle
closes. SCM can refuse to watch a service marked for deletion (measured: a
re-arm that excludes `STOPPED`, on a `STOPPED` service), so `start`, `stop`,
`uninstall` and `kill` then decide on the state they read: the state they wait
for ends the wait, and any other is an error that says SCM refused to watch the
service because it is marked for deletion. A service deleted while a `stop` or
`uninstall` waits counts as stopped.

After upgrading goetia, run `goetia daemon install` again for every daemon
installed by an older version. `protocol: plain` services registered before this
shim existed have no recovery actions until then, so the new shim runs them but
SCM does not restart them. On every start, the shim logs one warning when it
starts under a service that a different goetia version registered and
`restart` is not `never`.

A `protocol: plain` daemon installed by goetia 0.2.x runs the `goetia-shim.exe`
that sat beside `goetia.exe`, not the shim under `%ProgramFiles%\Goetia`
([above](#the-service-host-on-windows)). Run `goetia daemon uninstall <id>` and
then `goetia daemon install` for each.

The shim writes its own failures and warnings to the daemon's log once it has
read the daemon's config, so they appear among the daemon's output. Before
that, or if the daemon's log cannot be opened, it writes to
`%ProgramData%\Goetia\logs\<id>.log`. It also writes each to the Windows Event
Log.

`%ProgramData%\Goetia\logs` is readable by SYSTEM and Administrators only, so
reading `<id>.log` needs an elevated shell. `goetia daemon install` makes the
file, and the account the daemon runs as may only append to it; the shim never
makes it, so a file that is missing is a failed write, and the Event Log still
has the line; `goetia daemon install` makes it again, even when the daemon is
otherwise up to date. `goetia daemon uninstall` keeps the file.

The shim holds `<id>.log` open while it runs, and a service with no `logs:` gives its daemon a copy of that handle
for its output when it is started. The shim never makes the file: it opens the name again when the file it holds
was deleted, or when it holds none (an open failed). So an install that changes the account (SCM does not restart
the service) leaves the running shim its log. An install that changes the account while the service is starting, or
has to make the file for a running service, leaves the old account named beside the new one in the file's
permissions until the next install.

If you delete `<id>.log` while the service runs, the daemon of a service with no `logs:` goes on writing into the
deleted file until you restart it, as with any file a running program holds. The shim's own lines go to the Event
Log; those it writes to `<id>.log` go to the new file once `goetia daemon install` makes one, except for a run
started before an earlier install changed the account, until that run restarts. Windows frees the name at once on
current NTFS. Where the delete is deferred until every handle on the file closes, the name stays and
`goetia daemon install` fails saying that something still has the log open (a run of the daemon, or a program
reading the log): end that, then install again.

A `logs:` that is inside `%ProgramData%\Goetia\logs` is refused at install, whichever file it
names (the folder is the shim's default logs, which a daemon's own account cannot make a file in).
Remove `logs:` to use the default log, or name a file elsewhere. A manifest that named
`%ProgramData%\Goetia\logs\<id>.log` as its `logs:` must drop it; its output then goes to the same
file. A service installed with that `logs:` by an earlier goetia keeps writing the default log.

Goetia before this change left `%ProgramData%\Goetia\logs` readable by every
user, and `goetia daemon install` refuses a folder it did not make this way. To
upgrade, stop the services an earlier goetia installed, delete
`%ProgramData%\Goetia\logs` (keep any log you want first), and run
`goetia daemon install` for each daemon again.

`sc query` shows the difference between your command failing and the shim
failing:

| What happened                                                | `WIN32_EXIT_CODE`, `SERVICE_EXIT_CODE` SCM records                             |
| ------------------------------------------------------------ | ------------------------------------------------------------------------------ |
| command exited `0`, or was stopped                           | `0`, `0`                                                                       |
| command exited `n`, not `0`                                  | `1066`, `n` (a service-specific code)                                          |
| the shim could not run the command, or its wait on it failed | a Win32 code: the OS error, `13` for an unreadable spec, `31` if there is none |
| the shim could not show the command's tree terminated        | `0`, `1003`                                                                    |

A shim that cannot report at all exits `160` or `1063`; SCM's record is then
its own. A shim started without a service id, or that cannot connect to SCM,
dies before it has connected, and SCM does not restart it. A shim that connected
and then cannot register its control handler exits `1063` after the connect.
That exact case is not measured; a service host that connected and exited
without any report was, and SCM recorded `1067` and restarted it under the
restart policy.

Under `on-failure` both failures are restarted, so a spec the shim cannot read
or a command that is missing retries at `restart-delay` indefinitely, as it
does for `protocol: native`. If the shim cannot terminate the command's job on
`stop`, it logs why and reports the stop with Win32 code `0`, because a
non-zero Win32 code would make SCM start a second copy; the service-specific
code `1003` is the record that the command may still be running.

The shim runs a `protocol: plain` command inside a Windows Job Object and requires
one. The command is created suspended and assigned to the job before it runs;
if the assignment fails, it is resumed anyway and runs outside any job until the
shim notices and kills it, and anything it started in that time sits outside any
job too. The shim then logs why and reports `0`, `1003`, as for a `stop` it
could not complete, because no kill of a tree outside a job shows its
descendants gone and SCM must not restart over a possible survivor. A `start`
that sees the service stop with that record is a failed start.

Before the shim reports the service stopped for any reason (the command exiting,
cleanly or not, or the shim's own wait on it failing), it has the job terminated.
A `stop` first asks the command to exit and waits for the job to drain, as
[`stop-action`](#stop-action) says, and then does the same before it reports. If the job cannot
be terminated, the shim reports `0`, `1003` as for a `stop` it could not
complete, whatever `stop` follows.

The shim reports the service as stopping (`STOP_PENDING`) from the moment the
command's end or a `stop` is decided until it reports stopped, so the service
never reads `running` while its job is being terminated. In that window
`goetia daemon status` and `list` show `unknown`, the state they show for any
transitional one. A `start` fails and says the service is stopping, that SCM may
start it again itself under its restart policy, and to run the `start` again
once the service has stopped; it does not queue a start behind the stop. A
`stop` waits for the stopping to finish and then exits `0`, unless the shim
recorded the incomplete stop below. A `stop` of a
running service that does not accept one fails and names `goetia daemon kill`;
it does not wait. A `stop` (or the stop of a `restart`) that arrives while SCM is
starting the service, as in a restart after a failure, fails and says the STOP
was not delivered and is not queued, and to run `stop` again once the service is
running; if the service accepts STOP by the time `stop` looks, it says the STOP
was not delivered and to run `stop` again. A `stop` that sees the service
running or starting again after it began to stop (SCM restarted it after a
failure, under its restart policy) fails and says so, and to run `stop` again;
it does not wait for the restart. SCM treats the `STOPPED` that follows as it
would one that follows `running`: a failure is restarted per `restart:`, a
stop is not.

A `stop` that finds that record fails: it exits non-zero and says the command's
process tree may still be running. So does the stop of a `restart`, which then
does not start. A `stop` with `--timeout 0`, and the stop of
`restart --timeout 0`, do not wait for the stop, and do not read the record.
`goetia daemon uninstall` stops the service first and refuses to
delete it over the record, the same way and on a re-run too, while the record
stands: it leaves the service stopped, disabled and not deleted, and says so.
It has no option that deletes over the record; if you know the tree is gone,
`sc.exe delete <id>` removes the service. Nothing else guards
against a possible survivor: a later `start`, `restart` or `sc start` launches a
second copy beside it. `goetia daemon status` and `list` show such a service as `stopped`, and so they
do one waiting out its `restart-delay`.

"Terminated" means the shim asked Windows to terminate the job and the call
succeeded. Windows does not report when that termination has finished, so a
restart or `start` can briefly overlap the previous run's processes while they
exit, and a resource they held (a listening port, a lock file) may still be
taken. Such a start fails like any other.

### What returning means

The platform's service manager reports the service as running, as far as it
knows — not that the service is ready to serve, which no service manager can
tell us. That reads as one guarantee and is three, because what each manager
confirms differs:

- **systemd** confirms the `exec` itself succeeded: a missing executable or a
  missing user comes back as a failed start. Under every budget, a `start`,
  `stop` or `restart` for which systemd enqueued no job is a failure (exit
  `1`), `uninstall`'s stop included, even where `systemctl` itself exits `0`.
  The one exception is a
  `stop` of a unit systemd cannot load and that is not running: systemd
  answers it "not loaded" with no job, since there is nothing to stop, and
  that stop succeeds.
- **SCM**, for a `protocol: plain` daemon, confirms that `goetia-shim` started
  your command: the shim reports running after the spawn, so a command that
  cannot start is a failed start under every `restart:` policy. A command that
  starts and then exits at once is a successful start, whatever its exit code,
  as with `systemctl start`; it is the command's exit, not `start`, that
  reports it. The shim itself failing after the spawn, with a failed wait on
  the command or an exit status it cannot read, is a failed start if `start`
  sees the service stopped first and a successful one if it sees it running
  first. A `start` that lands as SCM is restarting the command, and that
  restart fails (a missing `goetia-shim` or a broken account), is a failed start,
  as is one where `start` sees the service stop with the shim's `0`, `1003`
  record before it sees it running. A command no Job Object contains is always
  seen that way, as the shim never reports running for it; for a command that
  did run, the shim reports running first, so `start` succeeds if it sees that.
  For a `protocol: native` daemon, which speaks to SCM itself, a
  service that stops before it reports running is a failed start.
- **launchd** sits between the two: the pid `launchctl kickstart -p` reports
  is launchd's own fork, taken before the user process reaches its `exec`.

### When the wait runs out

**Exit `4`, not `1`:** neither success nor failure was established. goetia
stopped waiting; it did not cancel anything, so the request stands and the
daemon may still arrive. `goetia daemon status <id>` shows the manager's
current view.

A `start` of a daemon that is already running is where the three disagree
under a very short `--timeout`. launchd and SCM say "running" — launchd
before any request is needed, SCM in its reply to the start — and that is
the confirmation, so it exits `0` whatever the budget. systemd answers a start with a job, never a state, and confirms only
when that job completes: a `--timeout` shorter than its round trip — tens of
milliseconds — exits `4`.

`restart` spends **one budget across the whole operation**, and one per
daemon — `restart a b c --timeout 30s` promises each of the three 30s rather
than leaving `c` whatever `a` and `b` did not spend, and neither leg gets a
budget of its own. If that budget runs out, the start leg is **not issued**:
goetia does not start into an unconfirmed stop.

So a timeout during `restart` leaves the daemon in an indeterminate state —
it may or may not be stopped when you look, and a start may or may not have
been issued. Which case you are in is in the error message.

`restart --timeout 0` issues the stop and the start and, except for the macOS
stop below, confirms neither, which is what it is for. On systemd it is one
`systemctl restart --no-block`, one job, so no start can overtake the stop, and
a restart systemd refuses has changed nothing (exit `1`). systemd stops the
unit and starts it again, except while the unit is still starting: it folds the
restart into the start already running, so nothing is stopped and the daemon
comes up fresh from that start. On macOS and Windows it is a stop and then a
start. On macOS the stop waits for launchd to remove the job (see the list
above), so the daemon is down before the start is issued, and only the start
goes unconfirmed. On Windows a start refused in that window exits `4` rather
than `0`: the daemon may be the instance the stop is still taking down, may be
going down, or may never have moved.

**On Windows, `restart --timeout 0` will usually stop the daemon and not
restart it.** The start arrives while the service is still coming down and is
refused — for a `protocol: plain` daemon almost always, since its service reads
`RUNNING` until the shim has taken the stop, then `STOP_PENDING` until the
daemon's tree has exited or its `timeout` is over — which leaves the daemon stopped, at exit `4`. To restart a
daemon on Windows, let `restart` wait for the stop: leave `--timeout` at its
default, or give it a duration other than `0`.

## Killing a daemon

`goetia daemon kill <ID...>` ends each daemon's running process as a failure,
and returns once that process has exited and the manager no longer reports it:
on every platform, the `status` after a `kill` does not show the killed process.
It takes no `--timeout` and does not change boot-enablement. What follows is the
daemon's `restart` policy: an `always` or `on-failure` daemon comes back, a
`never` one stays down. To have the next process, run `goetia daemon start`
after `kill`: it returns once the daemon runs, starting it now if the policy's
delay has not yet. The restart does not depend on the `kill` command surviving,
since the signal is sent before it waits. Like `stop`, it needs elevation, and
`--json` is refused.

It addresses the service, never a bare pid. goetia asks the manager for the
service's process, holds it, asks again whether it is still the service's, and
signals that held process, so the signal never reaches a stranger that took its
pid:

| Platform | What it sends                                                                                                                                                                                                                           |
| -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| systemd  | `SIGKILL` to the unit's main process alone, through a pidfd; systemd cleans up the rest of the cgroup. The process is the unit's `MainPID`.                                                                                             |
| launchd  | `SIGKILL` to the job's process, through an audit token. The process is the pid `launchctl print` shows.                                                                                                                                 |
| Windows  | SCM has no kill-by-service call, so goetia terminates the service's process (exit code `1`), after confirming through SCM that it is still the service's. SCM sees an unexpected exit and applies the recovery actions, as for a crash. |

The process killed is the one the manager reported when `kill` began. If it
exited on its own in the moment before the signal landed, `kill` still succeeds
once the manager has recorded that end, and the restart policy decides what
follows, as it would had the process exited just before `kill` ran. goetia does
not go after a service that restarts meanwhile.

On Windows with `protocol: plain` the service's process is `goetia-shim`; its Job
Object ends the daemon and its descendants with it. Windows does not report when
that termination has finished, so the daemon's processes can briefly outlive the
return of `kill`, and a restart by SCM can overlap them.

`kill` waits for the process to exit and for the manager to record it, with no
time limit: the exit of a process that was sent `SIGKILL` is an event, not a wait
to bound. A process the kernel cannot end, one in uninterruptible sleep on a hung
mount or in a stuck driver, keeps it waiting, as it keeps `stop` waiting; Ctrl-C
ends the wait and the signal stands. On every platform, when the process has still
not exited a second after it was signalled, goetia prints
`<id>: waiting for process <pid> to exit` to stderr.

A daemon with no process, whether stopped or between runs, is an error: exit
`1` and "is not running", because nothing was killed. A launchd task whose
start launchd has accepted and not yet forked (`goetia daemon start`, then
`kill` at once) is running by `status`: its `kill` waits for the process, with
no time limit, and then ends it.

A task has no restart policy: its `kill` ends the run in flight and leaves its
schedule armed, and a task with no run in flight is "not running" like any
other daemon with no process. What it sends on each platform is in
[Stopping and killing a task](#stopping-and-killing-a-task).

## Uninstalling a daemon

`goetia daemon uninstall <ID...>` stops each daemon's run in flight, removes its definition, and reloads the
manager. It reads the installed artifact, so it works where the manifest is gone, and it needs elevation.

**`--no-stop`** is for a program that uninstalls its own daemon from inside a run and must not be ended by it.
`uninstall` then removes the definition and everything that would start another run, sends nothing to a run in
flight and waits for nothing of it. The caller may be the run itself, or a process it started. With several ids,
`--no-stop` applies to each. With no run in flight it is a plain `uninstall`: it does all that `uninstall` does,
so nothing is left behind for want of a run, except, on systemd, a service waiting to be restarted (below).

What is left while the run lives is the manager's to report, and goetia reports it as it is.

**systemd.** The unit files are gone, so the id is not installed, and the process is the caller's. systemd keeps a
loaded unit with no file (`LoadState=not-found`) while the process lives, and never restarts it.

- `--no-stop` sends nothing in any state: no stop and no signal, a unit with no process included.
- A run still in its start job is ended by a reload that drops its unit file, at systemd's default start timeout.
  For goetia's units that is a task's run (a service is past its start phase). So when systemd reports the run
  of the definition goetia is removing still in its start job, `uninstall --no-stop` first writes a stand-in unit
  of the same name under `/run/systemd/system/` (a tmpfs, so a reboot removes it) with no start timeout. It
  carries the `KillSignal=` and `TimeoutStopSec=` systemd was applying to the job, a drop-in's included, and
  goetia's `[X-Goetia]` section and the id, which is how goetia knows the file is its own. Any other stop setting
  a drop-in gave the job (`KillMode=`, `ExecStop=`, `SendSIGKILL=` and the like) is not carried: a hand
  `systemctl stop` of the left run uses systemd's defaults for those. A hand `systemctl start` of the stand-in
  fails. A start job that is queued and not yet running (systemd reads the unit `inactive` with a job
  pending) gets the stand-in too. If it began before the reload, its run is kept; if it was still queued at the
  reload, it runs the stand-in, fails on its assertion and starts nothing, and the stand-in stays until the next
  `install` or `uninstall` of the id or a reboot. A run past its start phase needs none and is left as it is. A
  task whose job is gone and whose timer alone is installed gets none either: the unit under its name may be
  somebody else's.
- A service that is waiting to be restarted is not restarted, and nothing is sent to it. The reload fails the
  restart on the missing unit, so systemd lists the unit as `not-found failed`, and the machine reads `degraded`,
  until `systemctl reset-failed <id>` or a reboot. If systemd restarted it before the reload, that run began after
  goetia read the unit: nothing is sent to it and nothing restarts it again; it gets no stand-in, so one that
  has not finished starting when the manager reloads is ended by systemd at its start timeout. To cancel the
  restart instead, uninstall without `--no-stop`.
- Nothing removes the stand-in while its run has a process. The next `install` or `uninstall` of the id, an id
  nobody installed included, removes it once its run is over, reloads the manager, and clears a failed run of it
  with `systemctl reset-failed`. `uninstall --no-stop` of an id whose run has a process leaves the stand-in and
  the run. `uninstall` without the flag of an id nobody installed stops the run, clears it with
  `systemctl reset-failed` only if the stopped run failed, then removes the file. Once the id is installed again,
  the installed file outranks the stand-in, which is stale. The next `install` that rewrites the unit and the
  next `uninstall` of the id, with or without the flag, remove it whatever the run is doing (with `--no-stop`, a
  run still in its start job gets a fresh one), and clear nothing of the run, which is the installed job's. An
  `install` that finds the definition up to date does not touch it: the installed unit shadows it, and a reboot
  removes it. An `uninstall --no-stop` that fails after writing the stand-in leaves it, and it is then removed as
  above: while goetia's unit file is still installed the file shadows it, and with the file gone the stand-in is
  the unit's definition. If another program put its own unit file at the id during the call, goetia refuses the id
  as not its own, and the stand-in stays in `/run/systemd/system/` until a reboot, until you remove it, or until
  that file is gone and the next `uninstall` of the id removes it.
- A file at the stand-in's path that is not goetia's is never removed. `uninstall --no-stop` of an id whose job
  goetia removes refuses with an error naming the path, whether or not a run is in flight, and before anything
  is changed, the timer included. Remove the file, or uninstall without `--no-stop`. For a task whose timer alone
  is installed (it gets no stand-in), for an id nothing is installed at, or for `uninstall` without the flag, the
  file is left alone and changes nothing.
- `uninstall` of an id nothing is installed at stops nothing, with or without `--no-stop`, except the run a
  stand-in of goetia's keeps, which `uninstall` without the flag stops. Whatever else is left running is the
  caller's process, and `systemctl stop <id>` ends it. It reloads the manager when systemd still holds a
  definition loaded from goetia's own unit path whose file is gone (a `--no-stop` uninstall whose last reload
  failed), so the retry finishes it and the unit is not restarted from memory; it does no more for it, since
  goetia cannot show that a definition whose file is gone was its own. Where no manager can be asked it reads
  nothing and answers "not installed".
- `systemctl reset-failed` clears a unit that failed, even when its file is gone: such a unit is in memory. A unit
  that is not in memory has nothing to clear. goetia clears only what it can tell is its own: the failed run of a
  stand-in it removes. A left run that fails later stays listed as
  `not-found failed`, and the machine reads `degraded`, until `systemctl reset-failed <id>` or a reboot.
- An `install` of the same id while the left run lives is not refused: systemd adopts the process under the new
  definition until it exits. The left run is then governed by the new definition, its start timeout included,
  which can end a run that is still starting.
- A run somebody else starts between goetia's reading that a run is in flight and the reload is that caller's own
  conflicting action, and goetia does not guard against it.

## Exit codes

Two rules come before the numbers:

1. **A command exits `0` when the state of the system is as you asked for it
   to be.**
2. **`0` also means "I answered your question."** This is why a _partial_
   answer is `4` and not `0`. Returning `0` for an incomplete result makes
   `goetia daemon list && next-thing` silently wrong — the same reason
   `grep` has distinguished "no match" from "could not read the file" since
   v7 Unix.

| Code  | Name          | Meaning                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               | Anchored to                                                                                               |
| ----- | ------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| `0`   | success       | The state is as asked, or the question was fully answered.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            | —                                                                                                         |
| `1`   | error         | An operation was attempted and failed, or was refused outright. `kill` of a daemon with no process is this: nothing was killed.                                                                                                                                                                                                                                                                                                                                                                                                                                       | —                                                                                                         |
| `2`   | usage         | The command line was rejected before anything ran — by clap, or by one of `dispatch`'s refusals (`--json` on a subcommand that does not implement it; a wait flag on an `install` with no `--start`).                                                                                                                                                                                                                                                                                                                                                                 | clap's own default: the same code bash and argparse use for "the parser, not the program, rejected this". |
| `3`   | drift         | A determinate "installed state differs from the manifest" answer. Only `diff` returns it.                                                                                                                                                                                                                                                                                                                                                                                                                                                                             | Nothing; app-specific.                                                                                    |
| `4`   | indeterminate | A question goetia could not answer about an id: its state, whether it is occupied at all, or the outcome of a request goetia did not wait out — it stopped waiting, never waited, or lost track of it — or whether a request reached the manager at all, when goetia lost the tool carrying it after it may have started; or whether a start the manager accepted took effect, when its state then read back as not established; or, on launchd, which definition a job loaded during `install` holds, or what took a plist's place after `install` booted out a job. | The LSB init-script convention's "service status unknown".                                                |
| `5`   | conflict      | An installed artifact was modified outside goetia and `--force` was not given. See below: `--force` is not always the remedy.                                                                                                                                                                                                                                                                                                                                                                                                                                         | Nothing; app-specific — which is why `5` is the code that moved rather than usage errors.                 |
| `run` | the run's own | `daemon run` of a task, once the run has ended: the program's exit code, or `128` plus the signal that ended it. A program's `1` and `4` and goetia's own are indistinguishable. When goetia could not run the task, or could not tell how the run ended, it exits `1` or `4` as above. Outside the precedence below.                                                                                                                                                                                                                                                 | The program's own status.                                                                                 |

`2` and `4` are the two that must not be renumbered for tidiness: both are
tied to a convention outside goetia.

### Precedence

When more than one outcome applies in the same run, the winner is:

```
1 > 4 > 5 > 3 > 0
```

**This is a rule about which outcome wins, not an ordering of the
integers** — `5` outranks `3` despite being the larger number, and `1`
outranks `4` despite being the smaller. The precedence is the declaration
order of `ExitCode`. Never take `max()` over the integers. `2` never
enters the ladder: the refusals that produce it always happen alone,
before anything else could occur in the same run.

**Indeterminate outranks conflict** on purpose. A script that branches on
conflict re-runs with `--force`, and forcing on an incomplete picture is
worse than being told to re-run elevated. **Branch on `4` before you branch
on `5`.**

**`--force` is offered only where `--force` resolves it.** Goetia rewrites
`<id>.service` and clears its own `/etc/systemd/system/<id>.service.d`, and
nothing else — so a drop-in under another search root (`/usr/lib`,
`/etc/systemd/system.control`, where `systemctl set-property` writes) survives
the overwrite, and the run after it reports the identical conflict. Both
`install` and `diff` say which case a conflict is: either "re-run with
`--force` to overwrite", or the paths to remove by hand followed by
`systemctl daemon-reload`. The exit code is `5` for both — it is a conflict
either way, and only the remedy differs — so a script that branches on `5`
alone and forces unconditionally can loop. Read the message.

### What a consumer can rely on

**`diff`** returns `0`, `3`, `4`, `5`, or `1`.

- It exits `0` **if and only if** the daemon already matches the manifest.
- It exits `3` when the daemon would change, and `5`, `4` or `1` when it
  would conflict, could not be determined, or errored.

**Id verbs.** `uninstall` alone treats an already-absent artifact as
success; the other six keep it as a plain failure.

"Absent" means the **id** is empty, not that one file is missing. A systemd
unit whose fragment is gone but which still has a `<id>.service.d/*.conf`
drop-in, or a `multi-user.target.wants/<id>.service` link keeping it enrolled
at boot, is not absent: `uninstall` reports it as foreign and exits `1`,
naming what is left, exactly as `install` refuses the same state. Goetia
removes neither — a drop-in is as plausibly an administrator's override of a
unit shipped in `/usr/lib` as it is goetia's own leftover.

`<id>.service.d` is looked for under every root of `systemd.unit(5)`'s System
Unit Search Path — `/etc/systemd/system.control`, where `systemctl
set-property` writes, through `/usr/lib/systemd/system`. A drop-in under any
of them is drift; only goetia's own `/etc/systemd/system/<id>.service.d` is
ever removed by a write.

The artifact is `<id>.service` plus `<id>.service.d`, and nothing else.
Systemd reads more — `my-.service.d` for `my-daemon.service`, and a top-level
`service.d` for every service unit on the host — and goetia deliberately does
not scan those: they are named for a family of units rather than for one id,
so calling them a conflict would be untrue, and `--force` cannot clear a
directory that governs unrelated units. The cost is that an override
deliberately aimed at a goetia daemon through one of those directories is not
reported; seeing it takes a "what will actually run here" check, which is a
different question from drift.

| Verb                                 | Absent artifact | Why                                                                                                                            |
| ------------------------------------ | --------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `uninstall`                          | **0**           | Artifact absence is exactly what it asks for.                                                                                  |
| `stop`                               | 1               | A running unit whose fragment was deleted keeps running, so `stop x && echo "confirmed down"` would print that with `x` alive. |
| `disable`                            | 1               | Disabling after the fragment is gone is impossible, leaving a dangling `.wants` symlink: exit 0 while still enabled at boot.   |
| `start`, `restart`, `enable`, `kill` | 1               | Cannot act on what is not there.                                                                                               |

That table is about an absence goetia **established**. An id whose absence
it could not establish is `4` for all seven verbs, `uninstall` included: what
is at the id was not learned, which is one condition with one remedy for
every one of them. That is not always all that happened: a `restart` whose
start leg meets such an id has already issued its stop, and says so.

`4` covers two distinct states, and the difference is what goetia
established. **Ownership proven, contents not:** an artifact goetia owns
whose bytes it cannot read or decode. **Neither proven:** the read that
would have said whether anything is installed at that id did not complete at
all — on systemd, an `<id>.service` fragment an unelevated caller cannot
open, or, where no fragment is there to read, the `<id>.service.d` and
`multi-user.target.wants` scan that decides whether the id is empty. Goetia
claims no ownership in the second case, and does not suggest uninstalling.

Where each verb surfaces it: `status <id>` as that id's own `undetermined`
kind; `list` and `status` with no ids in the document's
[third key](#undetermined); `diff`, `show`, `install` and all seven id verbs
on stderr, having no `--json` document. Which verb returns which code for
which outcome is `dispatch`'s doc comment in `src/cli.rs` — the one place
the vocabulary is written down, and the authority this chapter summarises.
`install` therefore agrees with `diff` about an id neither could classify,
and the two differ only over an artifact that _was_ read and would not
decode, which `install` reports as `1` because the install genuinely did not
happen.

These classes are **disjoint, and they are separated by what goetia
_established_, not by what went wrong.** `not-installed` means absence was
proven; `foreign` and `unusable` mean presence was proven; `undetermined`
means neither was — the read that would have settled it did not complete, so
goetia claims nothing either way. A permission denial is therefore never
`foreign`: proving that a file exists is not proving whose it is, and the
read that would have said — the fragment's own, or a drop-in directory's —
is the one that did not complete. `foreign` needs a read that finished. And an error always wins — a run naming both
an absent id and an unusable one exits `1`, not `4`.

## `show`

`goetia daemon show [ID...]` and `goetia daemon show -f FILE` render a
daemon's resolved spec as YAML. With `-f` it reads a manifest, which names one
daemon, so `-f` takes no id; without, it reads the spec back out of what is
actually installed. Four guarantees:

1. **When both paths can see the daemon**, `show <id>` and
   `show -f <file>` render the same resolved spec identically, byte for
   byte. The agreement is conditional, not unconditional: without `-f`,
   `show` enumerates installed services, which cannot describe a unit this
   privilege level could not read. Such a unit reaches `show` as an
   `undetermined` entry, so `show <id>` reports that it could not determine
   the id's state and exits `4`, while `show -f <file>` still renders
   it from the manifest. On a manifest with a `backend-specific:` block,
   the spec `show` renders is the merged, host-effective one — `show -f`
   is deliberately not a transcription of the file, because it renders
   what would be installed _here_. Guarantee 1 survives that unchanged:
   both paths merge for the same host's native backend, so `show <id>`
   and `show -f <file>` still agree byte for byte.
2. Neither path ever checks elevation.
3. `show -f` touches no service manager; `show` without `-f` touches no
   manifest.
4. Output is YAML, one `# <id>` header per daemon, blank-line separated.

A daemon this privilege level _can_ see but whose stored spec will not
decode is a different case from "not installed": `show` exits `4` for it —
both per id and in the no-ids form. "Not installed" stays `1`, a determinate
answer, and outranks `4` when a single call names both kinds of id.

What `show` never does is call an id absent on a listing that did not
establish absence. An id `list()` reported as undetermined is `4`, and so is
**any** id `show` cannot find while an aggregate entry — one with no name —
is present, since such an entry may stand for that very id.

## What goetia can and cannot see

`list`, `status`, and `show` report only what the **current privilege level
can read** — and where a read that would have classified an id did not
complete, they **say so and exit `4`** rather than leaving the id out. An
enumeration that drops an id it could not read is indistinguishable from one
where that id does not exist, which is how an unelevated `goetia daemon list`
could once exit `0` with an empty document on a host that did have goetia
daemons installed.

The same holds one level up, for the enumeration itself: a directory or
registry scan that stops part-way keeps everything it had already classified
and adds one unnamed `undetermined` entry for whatever it never reached,
instead of failing the whole listing.

### The boundary that is guaranteed, and the two that are not

What is guaranteed is **one boundary: an artifact goetia reached and could
not read.** A unit file, plist, drop-in directory, registry key or service
object that the scan named and a denial — or an `EIO`, or a corrupt hive —
refused is **accounted for** in `undetermined` at exit `4`, never omitted.
All three backends hold that line, and `manager::conformance` asserts it on
each.

**Accounted for is not the same as named**, and the difference is what a
consumer has to script against. An entry names its id whenever it stands for
exactly one; where a backend cannot name it, the id is covered by an
aggregate entry with `name: null` instead. That is not a lesser guarantee,
but it is a different one: a `null` name blocks every negative conclusion
about every id, so the entry may be standing for the very id you are asking
about. `manager::conformance` asserts the `list` half in exactly those terms
— the id is named, **or** some aggregate entry is present — and separately
that no entry claims the id as goetia's, which a failed read never
established.

So a name-keyed lookup is only sound once `undetermined` holds no `null`
name. **Check for an aggregate before searching by name**: an unelevated
Windows `daemon list --json` returns `undetermined: [{"name": null, …}]` and
nothing else, and reading "no entry names `my-daemon`" as "`my-daemon` was
read and is absent" is wrong there — it may be installed and sitting behind
one of the denied reads that entry counts. See
[which reads count, per platform](#which-reads-count-per-platform) for which
backends produce which form.

It is **not** a guarantee that every installed id appears in every listing.
Two cases fall outside it, and in both the affected id is omitted with
nothing attached to _it_ — no `undetermined` entry under its name, nothing in
the document that distinguishes it from an id that really is absent:

- **An artifact that moves while goetia reads it.** goetia's own verbs move
  artifacts: `enable`/`disable` move a launchd plist between two directories,
  and `install` renames a systemd fragment aside before putting the new one
  in place (`replace_unit_verified` in
  `src/backend/systemd/manager/write.rs`). For that moment `<id>.service`
  genuinely does not exist, and a concurrent read of it answers "nothing at
  this id" — `list` for an id its scan had already named, `status` for the
  exact path it opens. No rate is published for this: the window is a single
  rename, its width depends on the host's filesystem and load, and a number
  measured on one box is not a budget another can plan against. Closing it is
  known follow-up work, tracked separately. Until it ships, a listing you
  intend to draw a negative conclusion from must not overlap a goetia verb
  running against the same host.
- **An id whose only trace is an enablement link.** The systemd scan takes id
  names from `<id>.service` and `<id>.service.d` and never from
  `multi-user.target.wants`, so an id whose sole trace is a link there is
  named by no pass and omitted from the listing. The case needs an id with
  nothing else to find: no `<id>.service` in `/etc/systemd/system` and no
  `<id>.service.d` under any of the twelve drop-in roots.

  `list` does still _stat_ `multi-user.target.wants/<id>.service` under four
  roots, for each id the scan named that has no fragment of its own — so
  where such a directory is not searchable, the three verbs disagree:
  `status <that-id>` answers `undetermined` at exit `4`, `show <that-id>`
  says "is not installed" at exit `1`, and `list --json` exits `4` carrying a
  named `undetermined` entry for each of _those_ ids — never one for the
  affected id, which no pass named. The exit `4` is real, but every name on
  it belongs to some other id, so a listing can be loud and still leave this
  one indistinguishable from absent. `undetermined` comes back empty
  alongside it only when no scanned id needed such a stat at all. Deliberate
  and bounded — the argument for why both ways of closing it cost more than
  the hole is on `HostScan` in `src/backend/systemd/manager.rs`.

**So the rule to script against, with its exception stated:** an id missing
from `daemons` may be read as uninstalled once no `errors` entry has a `null`
`id`, no `undetermined` entry has a `null` `name`, and neither key holds an
entry naming that id — **unless** a goetia verb ran concurrently against this
host, or the id could be traced only through an unsearchable
`multi-user.target.wants`. Neither is a condition a listing can detect, which
is why they are published here rather than reported there.

Note what this does _not_ require: that either key be empty. Entries naming
_other_ ids are the normal state of an unelevated listing on every platform,
and they leave the ids they do not name answerable.

**And the rule that holds unconditionally:** while an entry with a `null`
name is present in `undetermined`, no negative conclusion about any id is
sound — see [`undetermined`](#undetermined).

### Which reads count, per platform

**Bytes that were obtained are never `undetermined`.** goetia writes UTF-8
and nothing else — ini on systemd, XML on launchd — so an artifact whose
bytes will not decode is positively _not_ one of goetia's. That is a fact
about presence, so it is answered `foreign` and omitted, exactly as an
unmarked artifact is. The cost is accepted deliberately: one of goetia's own,
corrupted after the fact, now reads as a stranger's. Ownership lives in the
marker and the marker is in the bytes that would not decode — and the
alternative is a permanent, unclearable exit `4` for every vendor artifact on
the host that is not UTF-8.

Per platform:

- **systemd** — an `<id>.service` fragment the caller could not **open** is
  reported as `undetermined`, named. A fragment that _was_ read and is not
  UTF-8 is foreign and omitted, per the rule above; so is a symlink (what
  `systemctl mask` leaves), a FIFO, a device node or a directory, none of
  which is a unit file goetia wrote. The fragment is not the id: an
  `<id>.service.d` drop-in that could not be read is reported the same way,
  named, **with no fragment at all** — that read is what would have told
  goetia whether anything occupies the id, so `status` and `list` answer it
  identically instead of one saying "cannot determine" while the other
  leaves the id out. A task's `<id>.timer` is read as part of its id: a `<id>.timer`
  that is not goetia's makes the id foreign when goetia's own unit is a task, or
  absent, and an unreadable one beside a task is a failed read of the id. Beside
  a service, a timer that cannot be read or decoded is ignored, as before.
- **launchd** — a plist whose bytes could not be **obtained** is reported as
  `undetermined`, named. Bytes that were obtained and are not UTF-8 XML are
  foreign and omitted, per the same rule — a binary plist (`bplist00`, what
  `plutil -convert binary1` and `defaults write` produce by default, so a
  `/Library/LaunchDaemons` full of them is the normal state of a macOS host),
  a UTF-16 plist, or one stray Latin-1 byte. systemd and launchd answer this
  question identically. A task's timer plist is read as its job's, never as an
  id of its own; a timer plist whose job plist is missing is the job's id,
  unusable.
- **SCM** — an unelevated read of a service's `Parameters` is commonly denied, which goetia's Windows design assumes is the usual case rather than a measured one, so
  an unelevated `goetia daemon list` on Windows is **expected** to carry one
  `undetermined` entry standing for every service it could not inspect, and
  to exit `4`. That is the designed behavior, not a defect to report:
  re-running elevated is what empties it. How that one entry is shaped
  depends on how many services it stands for. For more than one — the usual
  unelevated case — it is an aggregate: `name: null`, carrying a count
  rather than a name, deliberately, since a listing can deny hundreds of
  reads and one entry cannot carry their names; that is why the `null`-name
  rule above exists at all. For exactly one it **names that service**, and
  its `reason` is about that service rather than about the host, so a
  `name == null` assertion written from the aggregate case is wrong on a
  host where exactly one read was denied. A task lives in Task Scheduler, not
  in SCM, and is read there; see [Status of a task](#status-of-a-task) for what
  an unelevated read of one reports. A Task Scheduler that cannot be read does
  not change what goetia does for services, installing a new one included: a
  listing then carries one unnamed `undetermined` entry for the task store,
  installing a task fails, and an id with no service is undetermined, since a
  task may have it.

**Whether a `reason` also carries a remedy is platform-specific.** On systemd
and launchd it is facts only, and any remedy belongs to the per-id
`errors[].message`. SCM appends one: a named entry carries the same remedy
`status <that-id>` gives for the same failed read — one denied read gets one
sentence, whichever verb asked — and an aggregate, having no single errno to
go on, names re-running elevated as the usual one.

A standing non-empty `undetermined` is not unique to Windows. An unelevated
`goetia daemon list` on Linux carries one named entry per unit it cannot open,
and units shipped `0600` (those carrying `LoadCredential=`, for instance) are
common; the same holds on macOS for a plist an unprivileged reader cannot
open. What is unique to Windows is the _aggregate_ form — one entry with a
`null` name standing for many ids — and the elevation-clears-it expectation
that comes with it.

### The two sources of `4`

Two within what `list`, `status` and `show` report. The mutating verbs reach
`4` four more ways: by
[waiting and giving up](#when-the-wait-runs-out); by losing track of a
request, one that reached the manager or one that may or may not have; for a
`restart` that does not wait, by a start refused after a stop nobody confirmed;
and, on launchd, by an `install` that wrote a plist and then found the job
loaded, or could not read whether it was, or that booted out a job and found
its plist taken
([above](#install-never-changes-whether-a-daemon-runs)). The two
sources are not interchangeable, and the remedies do not transfer:

- **`unusable`** — goetia's own daemon that goetia cannot use or report
  on. The marker was read; the blob would not decode, the spec in it can no
  longer be rendered — an account that no longer resolves, or a spec the
  backend now refuses — or the manager gave no live state for it — on
  systemd, a unit file the running manager cannot see.
  `goetia daemon uninstall <id>` is the remedy for the first two, and needs
  neither the blob nor a live state. It is not a remedy for the other: its
  `disable` goes through the manager too, and fails the same way, exit `1`,
  leaving the artifact where it was. Nor is `systemctl daemon-reload`, and
  the reason is what the case is: systemd loads a unit file it can see on
  demand, so having none for a name goetia found a file for means the two
  are not reading the same directory — goetia's own mount namespace, say —
  which no goetia verb changes.
- **`undetermined`** — a name goetia could not classify at all. The read
  that would have said whose it is never completed, so ownership is
  unestablished. Remedy: read it with more privilege.

Offering the first remedy for the second case would send someone to
uninstall what may be a stranger's service —
`HKLM\SYSTEM\CurrentControlSet\Services` and `/Library/LaunchDaemons` hold
every vendor's, not just goetia's. That is why goetia never claims ownership
of what it could not read, and why the two are separated by what was
_established_ rather than by what went wrong.

## Running the tests

Some tests change real system state: they install and remove real services and scheduled tasks (systemd timers, launchd
timer plists, Windows Task Scheduler tasks), write the registry,
`/etc/systemd/system` or `/Library/LaunchDaemons`, and create accounts. They run only in a sandbox
(a container, a VM or CI), and they are gated by two environment variables per group (`SCM` on
Windows, `SYSTEMD` on Linux, `LAUNCHD` on macOS):

- `GOETIA_TEST_<GROUP>`: unset or `1` runs the group's tests, `0` ignores them, anything else fails them.
- `GOETIA_TEST_<GROUP>_CONSENT`: a group's test that runs **fails** unless this is exactly `1`.
  Unset is `0`. Any other value fails it too.

A consented group also needs elevation (root, or an Administrator shell) and fails without it. On a
machine you care about, in any shell, set the group switches to `0` and run everything else (drop
`GOETIA_TEST_DEBUG_PRIVILEGE=0` where the token holds `SeDebugPrivilege`; see below):

```sh
GOETIA_TEST_SCM=0 GOETIA_TEST_SYSTEMD=0 GOETIA_TEST_LAUNCHD=0 GOETIA_TEST_DEBUG_PRIVILEGE=0 cargo nextest run
```

On Windows, some tests need a token holding `SeDebugPrivilege` (an elevated Administrator's
usually does) but change no system state, so they have a switch and no consent:
`GOETIA_TEST_DEBUG_PRIVILEGE` unset or `1` runs them, and they **fail** on a token without the
privilege; `0` ignores them.

On Linux and macOS, the tests of the directories `install` makes need root (they make directories root owns
and give files to other uids) but change nothing outside their own temporary directory under `/tmp`, so
they too have a switch and no consent: `GOETIA_TEST_ROOT_DIRS` unset or `1` runs them, and they **fail**
when the process is not root; `0` ignores them.

## License

[Apache-2.0](LICENSE.md)

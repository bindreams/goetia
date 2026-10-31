# Goetia

A cross-platform CLI and library that installs system daemons described in a
`goetia.yaml` manifest as native services: systemd on Linux, launchd on
macOS, and the Service Control Manager on Windows.

## Status

Pre-1.0. See the [releases page](https://github.com/bindreams/goetia/releases)
for what has actually shipped.

Everything documented below — the manifest's interpolation syntax, the
`.env` grammar, the `--json` document, and the exit codes — is **pre-1.0 and
may still change** between releases. Pin a specific released version, or a
specific commit if working ahead of one.

goetia requires **systemd 242+** (2019), and refuses to install, uninstall,
start, stop, restart, enable or disable anything on an older one, before
writing or running anything. It checks both the running systemd and the
`systemctl` client, which is what parses `--show-transaction`. A running
systemd it cannot ask is refused. Generated units use
`Type=exec` (240+), which is what lets a start report a missing executable or
user as a failure. An older systemd does not reject the directive: it logs that it
cannot parse it and runs the unit as `Type=simple`, silently losing exactly
that. And every `start` and `stop` runs `systemctl --show-transaction` (242+).

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

Every manifest field is interpolable except mapping keys and `user.id`:

- **Daemon ids** are map keys and are never interpolated. A `$` in one is an
  error: an interpolated id could collide with another daemon, or change
  which installed service an artifact belongs to.
- **`env` names** are map keys too, for the same reason — an interpolated
  name could silently overwrite another entry. `env` _values_ interpolate
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
selects the default.

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
  element, `user.name`, a daemon id — became the four-character text
  `null`. `env: {DEBUG: null}` installed a service whose `DEBUG` was the
  string `null`; `daemons: {null: …}` produced a daemon installed as
  `null.service`.
- **A container** — `daemons:`, `env:` or `command:` with nothing under it —
  became an _empty_ container. A manifest whose `daemons:` key had lost its
  body installed nothing and exited 0.

## An `env` key may not repeat

An `env` key that repeats, or that differs from another only in case, is
rejected rather than silently keeping one of the two. Case counts because
Windows looks an environment variable up case-insensitively, so
`{Path: a, PATH: b}` would reach the daemon as one variable — daemon ids
have been compared the same way, for the same reason, since before this.

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

## Directories: `install` prepares, it does not check

`install` creates a `cwd` and the parent directory of `logs` if they are
missing. A directory that already exists is not checked or changed: `install`
is not a start, and you may still be preparing it. On systemd an existing log
directory is never a problem for the account, because systemd opens the log
file as root before dropping privileges; a `cwd` the account cannot traverse
fails at `start` with `200/CHDIR`. A missing directory fails at `start` too,
which is why `install` creates it. The backends differ in what they create
and what they refuse:

- **systemd** creates every missing component and gives each one it created
  to a non-root `user`. A path that exists but is not a directory is refused.
- **launchd** creates every missing component and gives each one it created
  to a non-root `user`. An existing path of any kind, a dangling symlink
  included, is accepted, so a file where a directory belongs surfaces at
  `start`.
- **scm** creates nothing.

## Per-backend overrides

One manifest describes a daemon on all three platforms, but a path, an
account or a service name is rarely the same on all three. A daemon's
`backend-specific:` block holds a set of field overrides per backend, keyed
by `systemd`, `launchd` or `scm`:

```yaml
daemons:
  frpc:
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
          [
            'C:\Program Files\frpc\frpc.exe',
            -c,
            'C:\ProgramData\frpc\frpc.toml',
          ]
        cwd: 'C:\ProgramData\frpc'
        user: 'NT AUTHORITY\LocalService'
        env:
          RUST_LOG: debug
```

Every field but the daemon's id is overridable: `name`, `command`, `cwd`,
`env`, `user`, `restart`, `restart-delay`, `logs`, `type`. A scalar or list
field replaces the base value outright; `env` merges key by key, the
override winning where both set the same key. An unknown backend key is an
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
restart: on-failure
type: simple
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
resolves the manifest, so `show` works anywhere, for both `type: simple` and
`type: managed`; the `scm` backend refuses it when it generates the service,
in `install` and `install --dry-run`:

```console
$ goetia daemon install --dry-run -f goetia.yaml
error: daemon `frpc`: `restart: always` cannot be honoured by the scm backend: SCM recovery actions never fire after a clean exit. To keep `always` on systemd and launchd, set `backend-specific: scm: { restart: on-failure }`
```

`--dry-run` still previews the manifest's other daemons and exits `1`. Without
it, `install` over a manifest with several daemons installs the others and
reports the refused one, exiting `1`; under `--start` the refused daemon is not
started and the others are.
Override `restart: always` for Windows:

```yaml
restart: always
backend-specific:
  scm: { restart: on-failure }
```

A service installed by an earlier release with `restart: always` on Windows
is refused by `install` and `diff` as an unreadable artifact, until you run
`goetia daemon uninstall <id>` and then `goetia daemon install`. `status`
fails for it and `list` reports it as `unreadable`, because both render the
embedded copy of the spec and SCM now refuses it. `start`, `stop`, `restart`
and `uninstall` do not render it and keep working. What changes while it runs
depends on its `type`. A `type: simple` service is no longer restarted when
the command exits, because the new `goetia-shim` leaves every restart to SCM
and the service has no recovery actions. A `type: managed` service is unchanged: it already had
on-failure recovery actions, keeps them, and was never restarted after a clean
exit. To give a `type: simple` service recovery actions, add the override above
to the manifest, run `goetia daemon uninstall <id>`, then
`goetia daemon install`.

`${VAR}` is substituted only in the override for the backend being
installed. The other two are validated as authored, never substituted, so a
check that would need a resolved value is skipped rather than guessed at:
from Linux, `scm: {user: "${ACCT}"}` is unresolvable and therefore
unchecked, while `systemd: {user: "${ACCT}"}` is substituted and checked
like any other native value.

A host warns only about its native backend, and reads the warning from the
substituted native spec, base fields included. Moving `type`, `restart` or
`restart-delay` into `.env` does not silence a warning or an error the
literal would have produced. systemd has no warnings. launchd warns about a
`restart-delay` that is not a whole number of seconds. scm warns about
`type: managed` with `cwd` or `logs`, and about a `restart-delay` it must
clamp or round. A value written under a backend's own block is still
checked, and refused, from any host.

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
    { "id": "frpc", "state": "running", "enabled": true, "pid": 1234 }
  ],
  "errors": [],
  "undetermined": []
}
```

`daemons`, `errors` and `undetermined` are **always present**, as arrays,
possibly empty.

| Key                     | Type            | Notes                                                                                                                                                                                                                                                                             |
| ----------------------- | --------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `daemons[].id`          | string          | The daemon id.                                                                                                                                                                                                                                                                    |
| `daemons[].state`       | string          | One of `running`, `stopped`, `failed`, `unknown`.                                                                                                                                                                                                                                 |
| `daemons[].enabled`     | bool            | Whether the service is enabled at boot.                                                                                                                                                                                                                                           |
| `daemons[].pid`         | integer or null | `null` means the manager reports **no main process** — never "goetia could not find out", which is an `errors` entry instead.                                                                                                                                                     |
| `errors[].id`           | string or null  | The daemon id or service name the failure is attributable to; `null` for `unsupported`, and for an `unavailable` that belongs to no id.                                                                                                                                           |
| `errors[].kind`         | string          | See below.                                                                                                                                                                                                                                                                        |
| `errors[].message`      | string          | Human-readable detail.                                                                                                                                                                                                                                                            |
| `undetermined[].name`   | string or null  | The id the entry stands for. **`null` is one entry standing for many ids** — see below.                                                                                                                                                                                           |
| `undetermined[].reason` | string          | What did not complete, as facts: the operation, what it was on, and the failure. An aggregate's stands alone and names a count rather than an id. Whether it also carries a remedy is platform-specific — see [Which reads count, per platform](#which-reads-count-per-platform). |

A daemon's display `name` is deliberately absent: `status` has no spec to
read it from, and one field differing between the two subcommands would be
worse than sending you to `goetia daemon show`.

`errors[].kind` is one of eight values:

| `kind`          | Exit code | Meaning                                                                                                                                                                                                                                                                                                                                                                                         |
| --------------- | --------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `not-installed` | 1         | Nothing is installed at that id. Only from `status <id>`.                                                                                                                                                                                                                                                                                                                                       |
| `foreign`       | 1         | A read that _completed_ established that what is there is not goetia's: an artifact carrying no marker, or a masked unit's symlink. Never inferred from a read that failed. Only from `status <id>`.                                                                                                                                                                                            |
| `unreadable`    | 4         | Goetia read enough to know the id is its own, but cannot report on it — a blob it cannot decode, an embedded spec it can no longer render (an account that no longer resolves, or a spec the backend now refuses), or a live state it could not query, as of a unit systemd has not loaded. `message` says which.                                                                                                                                                                                                 |
| `undetermined`  | 4         | Goetia could not determine **whether** anything is installed at that id: a read it needed failed. Claims no ownership — that is the whole difference from `unreadable`. `message` names what would not read — a path, or on Windows a registry key or service object — and what would make it readable. Only from `status <id>`; out of `list()` the same fact is the `undetermined` key below. |
| `invalid-id`    | 1         | A command-line argument was not a valid daemon id. Fix the argument.                                                                                                                                                                                                                                                                                                                            |
| `unavailable`   | 1         | Obtaining the manager, or listing, failed, so **no** answer was obtained for any daemon — `id` is `null`. Or, under a daemon's `id`, goetia would not ask systemd about that daemon: offline, from a chroot, or where systemd did not boot.                                                                                                                                                     |
| `unsupported`   | 2         | `--json` was given to a subcommand that does not implement it.                                                                                                                                                                                                                                                                                                                                  |
| `other`         | 1         | Unreachable today; reserved so an unclassified failure has a home rather than being silently dropped.                                                                                                                                                                                                                                                                                           |

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

## Waiting for a start or stop

`daemon start`, `daemon stop`, `daemon restart` and `daemon install --start`
share one pair of flags:

| Flag                   | Meaning                                       |
| ---------------------- | --------------------------------------------- |
| `--timeout <DURATION>` | Wait up to `DURATION`. The default is `10s`.  |
| `--timeout 0`          | Issue the request and return without waiting. |
| `--no-timeout`         | Wait indefinitely.                            |

On macOS, `stop --timeout 0` is the exception: launchd has no request-only
stop, so it still runs `launchctl bootout` to completion, and the daemon is
down when goetia returns.

The two flags are mutually exclusive. `install` accepts them only alongside
`--start`: without it nothing is started, so there is nothing to wait for,
and the command line is refused at exit `2` having run nothing. Under
`--dry-run` they are inert, exactly as `--start` itself is.

**Durations** use the manifest's own grammar, the one `restart-delay`
accepts: `10s`, `500ms`, `2m30s`. Write a compound duration **unspaced**, or
quote it as a single argument. All four verbs take a variadic id list, so
`--timeout 2m 30s` passes `2m` to the flag and leaves `30s` to be read as a
daemon id — and goetia then reports that a daemon called `30s` is not
installed, which is a confusing answer to a typo the shell made.

### Restarts on Windows

Windows restarts a daemon through the service manager's recovery actions, for
`type: simple` and `type: managed` alike. `goetia-shim` runs your command once,
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
installed by an older version. `type: simple` services registered before this
shim existed have no recovery actions until then, so the new shim runs them but
SCM does not restart them. On every start, the shim logs one warning (in
`%ProgramData%\Goetia\logs\<id>.log` and as a warning in the Windows Event
Log) when it starts under a service that a different goetia version registered
and `restart` is not `never`.

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
does for `type: managed`. If the shim cannot terminate the command's job on
`stop`, it logs why and reports the stop with Win32 code `0`, because a
non-zero Win32 code would make SCM start a second copy; the service-specific
code `1003` is the record that the command may still be running.

The shim runs a `type: simple` command inside a Windows Job Object and requires
one. The command is created suspended and assigned to the job before it runs;
if the assignment fails, it is resumed anyway and runs outside any job until the
shim notices and kills it, and anything it started in that time sits outside any
job too. The shim then logs why and reports `0`, `1003`, as for a `stop` it
could not complete, because no kill of a tree outside a job shows its
descendants gone and SCM must not restart over a possible survivor. A `start`
that sees the service stop with that record is a failed start.

Before the shim reports the service stopped for any reason (the command exiting,
cleanly or not, or the shim's own wait on it failing), it has the job terminated.
A `stop` does the same before it reports. If the job cannot
be terminated, the shim reports `0`, `1003` as for a `stop` it could not
complete, whatever `stop` follows.

The shim reports the service as stopping (`STOP_PENDING`) from the moment the
command's end or a `stop` is decided until it reports stopped, so the service
never reads `running` while its job is being terminated. In that window
`goetia daemon status` and `list` show `unknown`, the state they show for any
transitional one. A `start` fails and says the service is stopping, that SCM may
start it again itself under its restart policy, and to run the `start` again
once the service has stopped; it does not queue a start behind the stop. A
`stop` waits for the stopping to finish and then exits `0`. A `stop` of a
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

Nothing guards against a possible survivor afterwards: `stop` exits `0`,
including the `stop` that produced the record, and a later `start`, `restart` or
`sc start` launches a second copy beside it. `goetia daemon status` and `list`
show such a service as `stopped`, and so they do one waiting out its
`restart-delay`. `goetia daemon uninstall` does not read the record either: it
deletes such a service, and a surviving command is left with no service.

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
- **SCM**, for a `type: simple` daemon, confirms that `goetia-shim` started
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
  For a `type: managed` daemon, which speaks to SCM itself, a
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

`restart --timeout 0` issues the stop and the start and confirms neither,
which is what it is for. On systemd it is one `systemctl restart --no-block`,
one job, so no start can overtake the stop, and a restart systemd refuses has
changed nothing (exit `1`). systemd stops the unit and starts it again, except
while the unit is still starting: it folds the restart into the start already
running, so nothing is stopped and the daemon comes up fresh from that start.
On macOS
and Windows it is a stop and then a start, and a start refused in that window
exits `4` rather than `0`: the daemon may be the instance the stop is still
taking down, may be going down, or may never have moved.

**On Windows, `restart --timeout 0` will usually stop the daemon and not
restart it.** The start arrives while the service is still coming down and is
refused — for a `type: simple` daemon almost always, since its service reads
`RUNNING` until the shim has taken the stop, then `STOP_PENDING` until its
child is gone — which leaves the daemon stopped, at exit `4`. To restart a
daemon on Windows, let `restart` wait for the stop: leave `--timeout` at its
default, or give it a duration other than `0`.

## Killing a daemon

`goetia daemon kill <ID...>` ends each daemon's running process as a failure.
On systemd and launchd it returns once the signal is sent; on Windows it returns
once SCM has recorded the exit: the service is then stopped, or already
restarted by its restart policy. It takes no `--timeout` and does not change
boot-enablement. What follows is the daemon's `restart` policy: an `always` or
`on-failure` daemon comes back, a `never` one stays down. Like `stop`, it needs
elevation, and `--json` is refused.

It addresses the service, never a bare pid:

| Platform | What it sends                                                                                                                                                                                                                           |
| -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| systemd  | `SIGKILL` to the unit's main process alone (`systemctl kill --kill-who=main-fail`); systemd cleans up the rest of the cgroup.                                                                                                           |
| launchd  | `launchctl kill SIGKILL system/<id>`.                                                                                                                                                                                                   |
| Windows  | SCM has no kill-by-service call, so goetia terminates the service's process (exit code `1`), after confirming through SCM that it is still the service's. SCM sees an unexpected exit and applies the recovery actions, as for a crash. |

On Windows with `type: simple` the service's process is `goetia-shim`; its Job
Object ends the daemon and its descendants with it. Windows does not report when
that termination has finished, so the daemon's processes can briefly outlive the
return of `kill`, and a restart by SCM can overlap them.

On Windows the process just terminated, or one already terminating (a second
`kill`, or a daemon dying on its own), is waited for until it exits, with no
time limit. When it has not exited yet, goetia prints
`<id>: waiting for process <pid> to exit` to stderr; a process stuck in a
driver request can keep it waiting.

A daemon with no process, whether stopped or between runs, is an error: exit
`1` and "is not running", because nothing was killed.

## Exit codes

Two rules come before the numbers:

1. **A command exits `0` when the state of the system is as you asked for it
   to be.**
2. **`0` also means "I answered your question."** This is why a _partial_
   answer is `4` and not `0`. Returning `0` for an incomplete result makes
   `goetia daemon list && next-thing` silently wrong — the same reason
   `grep` has distinguished "no match" from "could not read the file" since
   v7 Unix.

| Code | Name          | Meaning                                                                                                                                                                                                                                                                                                                                                                                                                                                        | Anchored to                                                                                               |
| ---- | ------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| `0`  | success       | The state is as asked, or the question was fully answered.                                                                                                                                                                                                                                                                                                                                                                                                     | —                                                                                                         |
| `1`  | error         | An operation was attempted and failed, or was refused outright. `kill` of a daemon with no process is this: nothing was killed.                                                                                                                                                                                                                                                                                                                                | —                                                                                                         |
| `2`  | usage         | The command line was rejected before anything ran — by clap, or by one of `dispatch`'s refusals (`--json` on a subcommand that does not implement it; a wait flag on an `install` with no `--start`).                                                                                                                                                                                                                                                          | clap's own default: the same code bash and argparse use for "the parser, not the program, rejected this". |
| `3`  | drift         | A determinate "installed state differs from the manifest" answer. Only `diff` returns it.                                                                                                                                                                                                                                                                                                                                                                      | Nothing; app-specific.                                                                                    |
| `4`  | indeterminate | A question goetia could not answer about an id: its state, whether it is occupied at all, or the outcome of a request goetia did not wait out — it stopped waiting, never waited, or lost track of it — or whether a request reached the manager at all, when goetia lost the tool carrying it after it may have started; or, on launchd, which definition a job loaded during `install` holds, or what took a plist's place after `install` booted out a job. | The LSB init-script convention's "service status unknown".                                                |
| `5`  | conflict      | An installed artifact was modified outside goetia and `--force` was not given. See below: `--force` is not always the remedy.                                                                                                                                                                                                                                                                                                                                  | Nothing; app-specific — which is why `5` is the code that moved rather than usage errors.                 |

`2` and `4` are the two that must not be renumbered for tidiness: both are
tied to a convention outside goetia.

### Precedence

When more than one outcome applies in the same run, the winner is:

```
1 > 4 > 5 > 3 > 0
```

**This is a rule about which outcome wins, not an ordering of the
integers** — `5` outranks `3` despite being the larger number, and `1`
outranks `4` despite being the smaller. Never take `max()` over goetia's
exit codes. `2` never enters the ladder: the refusals that produce it always
happen alone, before anything else could occur in the same run.

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

- It exits `0` **if and only if** every selected daemon already matches the
  manifest.
- It exits `3` when at least one selected daemon would change **and nothing
  was indeterminate, conflicting, or errored**.

`3` is therefore **not a "drift is present" signal**. One daemon that would
be created alongside one that conflicts returns `5`, not `3`, because `5`
outranks `3`. A script that branches on `3` as "is there drift" silently
skips the conflicting daemon.

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
proven; `foreign` and `unreadable` mean presence was proven; `undetermined`
means neither was — the read that would have settled it did not complete, so
goetia claims nothing either way. A permission denial is therefore never
`foreign`: proving that a file exists is not proving whose it is, and the
read that would have said — the fragment's own, or a drop-in directory's —
is the one that did not complete. `foreign` needs a read that finished. And an error always wins — a run naming both
an absent id and an unreadable one exits `1`, not `4`.

## `show`

`goetia daemon show [ID...] [-f FILE]` renders a daemon's resolved spec as
YAML. With `-f` it reads a manifest; without, it reads the spec back out of
what is actually installed. Four guarantees:

1. **When both paths can see the daemon**, `show <id>` and
   `show -f <file> <id>` render the same resolved spec identically, byte for
   byte. The agreement is conditional, not unconditional: without `-f`,
   `show` enumerates installed services, which cannot describe a unit this
   privilege level could not read. Such a unit reaches `show` as an
   `undetermined` entry, so `show <id>` reports that it could not determine
   the id's state and exits `4`, while `show -f <file> <id>` still renders
   it from the manifest. On a manifest with a `backend-specific:` block,
   the spec `show` renders is the merged, host-effective one — `show -f`
   is deliberately not a transcription of the file, because it renders
   what would be installed _here_. Guarantee 1 survives that unchanged:
   both paths merge for the same host's native backend, so `show <id>`
   and `show -f <file> <id>` still agree byte for byte.
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
  leaves the id out.
- **launchd** — a plist whose bytes could not be **obtained** is reported as
  `undetermined`, named. Bytes that were obtained and are not UTF-8 XML are
  foreign and omitted, per the same rule — a binary plist (`bplist00`, what
  `plutil -convert binary1` and `defaults write` produce by default, so a
  `/Library/LaunchDaemons` full of them is the normal state of a macOS host),
  a UTF-16 plist, or one stray Latin-1 byte. systemd and launchd answer this
  question identically.
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
  host where exactly one read was denied.

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

- **`unreadable`** — goetia's own daemon that goetia cannot use or report
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

## License

[Apache-2.0](LICENSE.md)

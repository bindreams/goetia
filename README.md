# Goetia

A cross-platform CLI and library that installs system daemons described in a
`goetia.yaml` manifest as native services: systemd on Linux, launchd on
macOS, and the Service Control Manager on Windows.

## Status

Early development. No release yet.

Everything documented below — the manifest's interpolation syntax, the
`.env` grammar, the `--json` document, and the exit codes — is **pre-1.0 and
may still change**. There is no release to pin yet; until there is, script
against a specific commit.

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
This is the one place a manifest that was previously accepted can now fail:
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
  "errors": []
}
```

`daemons` and `errors` are **always present**, as arrays, possibly empty.

| Key                 | Type            | Notes                                                                                                                                              |
| ------------------- | --------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| `daemons[].id`      | string          | The daemon id.                                                                                                                                     |
| `daemons[].state`   | string          | One of `running`, `stopped`, `failed`, `unknown`.                                                                                                  |
| `daemons[].enabled` | bool            | Whether the service is enabled at boot.                                                                                                            |
| `daemons[].pid`     | integer or null | `null` means the manager reports **no main process** — never "goetia could not find out", which is an `errors` entry instead.                      |
| `errors[].id`       | string or null  | The daemon id or service name the failure is attributable to; `null` for exactly the `unavailable` and `unsupported` kinds, which belong to no id. |
| `errors[].kind`     | string          | See below.                                                                                                                                         |
| `errors[].message`  | string          | Human-readable detail.                                                                                                                             |

A daemon's display `name` is deliberately absent: `status` has no spec to
read it from, and one field differing between the two subcommands would be
worse than sending you to `goetia daemon show`.

`errors[].kind` is one of eight values:

| `kind`          | Exit code | Meaning                                                                                                                                                                                                                           |
| --------------- | --------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `not-installed` | 1         | Nothing is installed at that id. Only from `status <id>`.                                                                                                                                                                         |
| `foreign`       | 1         | Something exists there that goetia does not own, or that this privilege level cannot read. Only from `status <id>`.                                                                                                               |
| `unreadable`    | 4         | Goetia read enough to know the id is its own, but cannot report on it — a blob it cannot decode, or a live state it could not query. `message` says which.                                                                        |
| `undetermined`  | 4         | Goetia could not determine **whether** anything is installed at that id: a read it needed failed. Claims no ownership — that is the whole difference from `unreadable`. `message` names the path and what would make it readable. |
| `invalid-id`    | 1         | A command-line argument was not a valid daemon id. Fix the argument.                                                                                                                                                              |
| `unavailable`   | 1         | Obtaining the manager, or listing, failed, so **no** answer was obtained for any daemon.                                                                                                                                          |
| `unsupported`   | 2         | `--json` was given to a subcommand that does not implement it.                                                                                                                                                                    |
| `other`         | 1         | Unreachable today; reserved so an unclassified failure has a home rather than being silently dropped.                                                                                                                             |

### Exit code

The exit code is **zero exactly when `errors` is empty**, and otherwise the
precedence-max of the codes in the table above (see
[Exit codes](#exit-codes) for the precedence rule).

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
determined: parse stdout first, then read `errors`.

## Exit codes

Two rules come before the numbers:

1. **A command exits `0` when the state of the system is as you asked for it
   to be.**
2. **`0` also means "I answered your question."** This is why a _partial_
   answer is `4` and not `0`. Returning `0` for an incomplete result makes
   `goetia daemon list && next-thing` silently wrong — the same reason
   `grep` has distinguished "no match" from "could not read the file" since
   v7 Unix.

| Code | Name          | Meaning                                                                                      | Anchored to                                                                                               |
| ---- | ------------- | -------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| `0`  | success       | The state is as asked, or the question was fully answered.                                   | —                                                                                                         |
| `1`  | error         | An operation was attempted and failed, or was refused outright.                              | —                                                                                                         |
| `2`  | usage         | The command line was rejected before anything ran — by clap, or by the `--json` refusal.     | clap's own default: the same code bash and argparse use for "the parser, not the program, rejected this". |
| `3`  | drift         | A determinate "installed state differs from the manifest" answer. Only `diff` returns it.    | Nothing; app-specific.                                                                                    |
| `4`  | indeterminate | A question goetia could not answer about an id: its state, or whether it is occupied at all. | The LSB init-script convention's "service status unknown".                                                |
| `5`  | conflict      | An installed artifact was modified outside goetia and `--force` was not given.               | Nothing; app-specific — which is why `5` is the code that moved rather than usage errors.                 |

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
exit codes. `2` never enters the ladder: the thing that produces it always
happens alone, before anything else could occur in the same run.

**Indeterminate outranks conflict** on purpose. A script that branches on
conflict re-runs with `--force`, and forcing on an incomplete picture is
worse than being told to re-run elevated. **Branch on `4` before you branch
on `5`.**

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
success; the other five keep it as a plain failure.

"Absent" means the **id** is empty, not that one file is missing. A systemd
unit whose fragment is gone but which still has a `<id>.service.d/*.conf`
drop-in, or a `multi-user.target.wants/<id>.service` link keeping it enrolled
at boot, is not absent: `uninstall` reports it as foreign and exits `1`,
naming what is left, exactly as `install` refuses the same state. Goetia
removes neither — a drop-in is as plausibly an administrator's override of a
unit shipped in `/usr/lib` as it is goetia's own leftover.

| Verb                         | Absent artifact | Why                                                                                                                            |
| ---------------------------- | --------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `uninstall`                  | **0**           | Artifact absence is exactly what it asks for.                                                                                  |
| `stop`                       | 1               | A running unit whose fragment was deleted keeps running, so `stop x && echo "confirmed down"` would print that with `x` alive. |
| `disable`                    | 1               | Disabling after the fragment is gone is impossible, leaving a dangling `.wants` symlink: exit 0 while still enabled at boot.   |
| `start`, `restart`, `enable` | 1               | Cannot act on what is not there.                                                                                               |

**`4` is returned by** `status` on an id it owns but cannot read, by `list`
for an entry goetia owns but cannot decode, and by `diff` and `show` when
the installed artifact cannot be read. `status` and `diff` also return it —
as the `undetermined` kind — when a read that would have said whether
anything is installed at that id failed at all, typically an
`<id>.service.d` drop-in directory an unelevated caller cannot open. Goetia
claims no ownership of such an id, and does not suggest uninstalling it.
`install` keeps that same case at `1`: there the operation is the install,
and it genuinely did not happen.

The three classes are **disjoint**: a permission denial and a foreign
service are never reported as absence, and absence is never reported as
either. And an error always wins — a run naming both an absent id and an
unreadable one exits `1`, not `4`.

### Compatibility note

**`4` is now reachable in text mode**, not only under `--json`. An
unreadable entry previously exited `1` from `list`, `status`, and `show`.

This is deliberate: the exit code is now computed identically with and
without `--json`, because a verb whose exit code depends on its output
format is exactly the split this work removed. It is still a change to a
non-`--json` path, so a script that treated `1` from `list` as "some entry
was unreadable" needs to accept `4` as well.

## `show`

`goetia daemon show [ID...] [-f FILE]` renders a daemon's resolved spec as
YAML. With `-f` it reads a manifest; without, it reads the spec back out of
what is actually installed. Four guarantees:

1. **When both paths can see the daemon**, `show <id>` and
   `show -f <file> <id>` render the same resolved spec identically, byte for
   byte. The agreement is conditional, not unconditional: without `-f`,
   `show` enumerates installed services, which silently skips a unit this
   privilege level cannot enumerate. For a daemon that is installed but
   unreadable unelevated, `show <id>` therefore reports "not installed" and
   exits `1`, while `show -f <file> <id>` still renders it from the
   manifest.
2. Neither path ever checks elevation.
3. `show -f` touches no service manager; `show` without `-f` touches no
   manifest.
4. Output is YAML, one `# <id>` header per daemon, blank-line separated.

A daemon this privilege level _can_ see but whose stored spec will not
decode is a different case from "not installed": `show` exits `4` for it —
both per id and in the no-ids form. "Not installed" stays `1`, a determinate
answer, and outranks `4` when a single call names both kinds of id.

## What goetia can and cannot see

`list`, `status`, and `show` report only what the **current privilege level
can read**.

Where goetia knows it could not determine something, it says so and exits
`4`. Where it cannot even tell that something was missed, it currently
**omits it silently**: an unelevated `goetia daemon list` can return an
empty document with exit `0` on a host that does have goetia daemons
installed.

Closing that gap is known work, planned separately. When it lands, those
entries will report `4` like every other indeterminate answer. Until then,
**run elevated to get a complete answer.**

## License

[Apache-2.0](LICENSE.md)

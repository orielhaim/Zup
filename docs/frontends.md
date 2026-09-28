# Installer frontends and the toolchain

The **developer CLI** (`zup`) is the tool a person installs. The **installer
runtime** is what a generated `Acme-Setup.exe` carries and what an installation
persists as `maintenance.exe`. They are two packages because a Cargo feature that
removed half of one binary was a feature that turned the product into something
else.

The runtime has three presentations over one engine:

- `gui` - the normal desktop experience, using the GPUI frontend.
- `console` - a Windows console application with Cliclack prompts and Indicatif
  progress.
- `headless` - non-interactive, for CI, images, remote sessions, and provisioning.

A presentation only presents the shared planner, lifecycle, transaction
coordinator, ownership ledger, authenticated worker, updates, plugins,
diagnostics, and recovery engine. It is a compile-time fact of the binary that
started the process, passed in explicitly: there is no global to set and no
process-wide override to read back.

## The toolchain a build composes from

`zup build` does not contain the runtime. It **composes** an installer out of
binaries zup itself produced:

| Component | What it is | Named for |
| --- | --- | --- |
| Runtime template | The native runtime a target's installer *is* | machine and presentation |
| Launcher | The bootstrapper a universal or thin artifact starts through | launcher experience and network capability |

A component's file name is not a compatibility check - `zup-setup-gui.exe` is
written by every zup release that ever had a GUI template. So every component
ships a machine-readable **descriptor** beside it:

```text
<component>.exe
<component>.exe.zup-toolchain.json
```

The descriptor names the contract version, the zup release that produced the
bytes, the machine, the presentation, the digest, and the size. The build then
confirms it against the file's own PE header. The two agreeing is what makes the
claim worth anything **on a build host that cannot run the file** - a Linux host
composes a Windows installer for two architectures and can start neither.

A resolved component is refused if it has no descriptor or the descriptor does not
parse; its contract version is not one this zup speaks; it was produced by a
different zup release; it is a runtime when a launcher was wanted or the other way
round; its machine, presentation, or launcher capability is not the one asked for;
or its bytes are not the size or digest the descriptor recorded.

An **offline** launcher composed into a thin artifact is refused rather than
accepted: it cannot resolve a release, so the installer it produced would refuse
to install itself on a user's machine - a failure with no build-time symptom, which
is the whole class of mistake this contract exists to catch.

### Producing one

```text
cargo xtask toolchain build            # for `cargo run` and `cargo test`
cargo xtask toolchain build --profile release
cargo xtask toolchain package [--profile dev|release] [--out <dir>]
```

`build` produces all seven components - three runtime templates for the host
machine and four launchers for `i686-pc-windows-msvc` - and stages them in
`target/<profile>/toolchain/<version>/` beside the `zup` executable.

`package` assembles the release: `zup.exe`, the component directory, and
`zup-toolchain.json` naming every file with its digest. It verifies the result
before returning, so a packaged release that reports success is one whose index was
read back and every file in it hashed. Unzip it anywhere and `zup build` works -
the resolver already searches `<exe dir>/toolchain/<version>`, which is exactly
where the components land.

The launcher is built as its own package with a deliberately small dependency
closure, which is the wrong trade for a 128 MB installer and the right one for a
launcher whose size is a design constraint. That is why it is not something
`cargo test` builds.

### Resolving one

`zup build` asks for a *semantic component* - "the console runtime for
`x86_64-pc-windows-msvc`" - and the resolver finds the bytes in a fixed
precedence, first match wins:

1. an explicit `--runtime` / `--dispatcher` path
2. an explicit toolchain root - `--toolchain <dir>`, or `ZUP_TOOLCHAIN`
3. the installed toolchain cache for this exact zup version
4. a toolchain staged beside this executable, versioned then unversioned

There is no network step, deliberately. A build that silently depends on GitHub
being reachable is a build a release engineer discovers is broken during an
outage, and a toolchain that has to be fetched is one whose provenance nobody can
state. Three of the four arms are somebody's decision; the fourth is the cache,
produced by:

```text
zup toolchain install <RELEASE> [--state-root <dir>] [--format <human|json>]
zup toolchain status           [--state-root <dir>] [--format <human|json>]
zup toolchain clean            [--state-root <dir>] [--all] [--dry-run]
```

`install` takes a release directory, refuses one from another zup version,
verifies every named file and descriptor cross-check, copies, and then verifies the
cache by resolving out of it the way a build will. `status` reports the seven
components a host needs and which arm answered each. `clean` removes every cached
version *except* this one's, because a machine can have two zup releases on it and
one deleting the other's components breaks it.

`cargo xtask release clean-room` proves the whole arrangement from outside the
repository - no checkout, no `target/`, no staged runtime, no `xtask`.

## Selecting a presentation

A manifest sets the default with a top-level `frontend = "console"`, and
`--frontend` on `zup build` takes precedence. The resolver asks for the matching
component, so a manifest that asks for a presentation with no staged template
fails with a message naming the component and the command that produces it, rather
than composing an installer that presents the wrong thing.

## The runtime's command surface

```text
install    apply this package, or bring an installed application up to it
modify     change which components an installed application has
repair     restore the resources this application owns that have drifted
update     resolve a verified release and install what changed
uninstall  remove the application and the resources it owns
```

`upgrade` is not a verb a person types: a person handed a newer `Acme-Setup.exe`
does not need to know whether that is an install or an upgrade, and neither does a
framework updater that runs it. `install` resolves the verb from the machine's own
record. The internal form exists as hidden `__upgrade` for a contract that has to
name it.

Recovery is an engine capability, not a command. An interrupted transaction is
reconciled from the record the engine wrote before it started, so a person never
reads a transaction identifier out of a log. The explicit form is hidden
`__recover`. The process boundaries are `__worker` and `__uninstall_runner`. None
appears in help.

The runtime's help is addressed to the file the person ran: a generated installer
is renamed to `Acme-Setup.exe` before anybody sees it, and help that says
`zup-setup-gui install` is a compile-time name leaking into a shipped product.

A persisted installation is `maintenance.exe`, not `Setup.exe`. The user-facing
installer is an installation medium; the executable that persists beside an
installed application is a maintenance runtime, and calling the second one the
first is how a support conversation starts in the wrong place. Apps & Features'
`UninstallString`, `ModifyPath`, and `DisplayIcon` all point at the persisted copy,
so a user who deletes the downloaded file has not deleted their own uninstaller.

## Build readiness

`zup doctor` answers whether `zup build` would succeed right now, resolving the
manifest, the target matrix, the runtime templates, and the output paths through
the same code the build uses.

```powershell
zup doctor --manifest zup.toml
zup doctor --manifest zup.toml --format json
```

It is read-only: it never writes an artifact, downloads a tool, or changes machine
state. Plugin sources are hashed and AOT-compiled in memory, and output directories
are probed for write access without creating anything. Every failing check across
every selected target is reported in one pass, and it exits nonzero when any check
fails.

`--target` is repeatable and accepts a profile name or a canonical triple. One
`--runtime` and one `--output` are required per selected target, exactly as in
`zup build`. A missing runtime, a missing output directory, or the wrong number of
inputs is reported as a diagnostic rather than a usage error, so one run shows
every problem.

The runtime-template row is one check rather than three, because a build refuses a
component that fails any part of it - a report that split one decision into three
shows a green row for a file the build would not use. The row's message names the
zup release and machine the component is for and says where it was resolved from,
so "which template did this build use" has a printed answer.

Only Windows targets have an implemented backend: a non-Windows target reports
`backend not implemented`, a Windows target on a non-Windows host reports `backend
unavailable`, and neither case produces an artifact.

`--format json` writes one result whose `details` carries the whole check table,
including the rows that were *skipped* - a skipped check is a question that was
never answered, and a report showing only failures would call a project ready on
the strength of checks that never ran. See
[the automation protocol](automation.md).

## Console behavior

A console build uses the normal terminal stream and keeps scrollback. With a real
TTY it prompts for scope, optional components, install location, and confirmation.
Simple manifests ask only for confirmation. Redirected stdin or stdout, and
`--non-interactive`, disable prompts: a front end that cannot ask has to decide,
and for a redirected stream the decision is the non-interactive one.

Ctrl-C requests cooperative cancellation and waits for a transaction boundary. The
terminal is restored after success, failure, panic, or cancellation.

## Headless behavior

Headless never prompts, reads stdin, opens a window, or requires a TTY.

```powershell
.\Acme-Setup.exe install --yes
.\Acme-Setup.exe install --scope machine --enable cli --install-directory C:\Apps\Acme --yes
.\Acme-Setup.exe modify --disable cli --yes
.\Acme-Setup.exe repair --yes
.\Acme-Setup.exe uninstall --yes
```

Machine-readable stdout contains only the selected output. Runtime diagnostics and
the session log stay on stderr or in the session log. The event stream uses
explicit DTOs, not serialized internal Rust structures:

```json
{"type":"started","protocol_version":1,"application":"Acme","version":"1.4.0","action":"install"}
{"type":"phase","state":"executing"}
{"type":"progress","phase":"files","completed":760,"total":1000,"label":"Installing files"}
{"type":"completed","outcome":"success"}
```

## Exit codes

Clap keeps conventional argument and usage behavior. Runtime outcomes use a small
stable set:

| Code | Outcome |
| ---: | --- |
| 0 | `success` |
| 2 | `cancelled` |
| 3 | `invalid_invocation` or `configuration` |
| 4 | `ownership_conflict` |
| 5 | `authorization_required` |
| 6 | `verification_failure` |
| 7 | `recovery_required` |
| 8 | `installation_busy` |
| 3010 | `reboot_required` |
| 1 | `failure` |

The same mapping appears in `outcome` for JSON and JSONL callers. The developer CLI
uses an exit code and stderr, and has no machine-readable failure envelope: there
is no consumer to read one, and an envelope nothing parses is a second place for a
message to be wrong.

## Elevation and Server Core

Interactive GUI and console sessions may use the existing authenticated UAC
worker. A non-interactive operation proceeds when it is already elevated. An
unelevated machine-scope operation fails immediately with exit code 5 and does not
show a credential or consent dialog. Run unattended machine installs from an
already elevated session.

The headless and console templates do not link GPUI or graphics/window
dependencies. They still use the Win32 APIs the backend requires. Treat Server Core
support as a tested compatibility claim: run the Windows multiprocess and security
suites on each supported image before publishing a package for it.

## Apps & Features

GUI packages register GUI Modify and Uninstall commands. Console packages register
console maintenance commands. Headless packages set the ARP `NoModify` and
`NoRepair` flags and register deterministic, non-interactive uninstall commands. All
maintenance files are the selected runtime, so a package never promises a GUI it
did not embed.

An entry zup did not write is not deleted. A person who edits an entry in Apps &
Features has expressed an intent, and an uninstall leaves their edit in place.

## Updates

All frontends use the same TUF client and verified download path:

```powershell
.\Acme-Setup.exe update check
.\Acme-Setup.exe update --yes
```

Headless update output can be consumed with `--output json` or `--output jsonl`; it
never asks an unexpected question. The downloaded package is checked for the
expected target and frontend before it is started. See [updates](updates.md).

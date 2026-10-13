# Platforms and targets

A target is an operating system and an architecture, named operating system
first: `windows-x64`, `windows-arm64`, `windows-x86`, `linux-x64`, `linux-arm64`.

A target profile binds a project source directory to a target. When the profile
name is a target, no `target` field is needed:

```toml
[build.targets.windows-x64]
source = { directory = "dist/windows-x64" }

[build.targets.windows-arm64]
source = { directory = "dist/windows-arm64" }
```

A profile with any other name states its target explicitly:

```toml
[build.targets.production]
target = "windows-x64"
source = { directory = "dist/production" }
```

Declare one profile per payload/target pair. `--target` accepts profile names or targets:

```bash
zup build --target windows-x64
zup build --target linux-x64
```

With no `--target`, commands select every declared profile where the command allows it.

A bare operating system is not a target: `zup build --target windows` is
refused rather than guessing an architecture.

## Universal artifacts

A universal artifact is a distribution containing several compatible targets,
selected at launch time. It is declared separately from targets:

```toml
[build.artifacts.windows]
targets = ["windows-x64", "windows-arm64"]
```

```bash
zup build --artifact windows
```

Here `windows` names the artifact, not a target. If several Windows variants
are configured, one Windows installer is produced whose dispatcher selects the
matching architecture at runtime. A universal artifact supports only the
architectures it was built with.

## Windows

Targets: `windows-x64`, `windows-arm64`, `windows-x86`. Frontends: `gui`, `console`, `headless`. Universal and thin artifacts are Windows installers.

A Windows build embeds the package into the runtime template as PE resources — plain file I/O, no Windows API on the build host.

## Linux

| Capability | Status |
| --- | --- |
| Target | `linux-x64` |
| Frontends | `gui`, `console`, `headless` (`gui` needs a Wayland or X11 session) |
| Scope | `user`, `machine`, `either` (`either` needs an explicit `--scope`) |
| Artifact | one self-contained installer per target (no universal) |
| Signing | no platform signature; authenticity is the artifact digest and release identity |
| Machine payload | `/opt/<App>`, `/var/opt` for shared data, `/var/lib/zup` for state |
| Machine services | systemd system units, machine scope only |
| User services | not supported |
| Desktop integration, PATH, protocols, file associations | user scope only; `desktop` launchers and PATH entries are refused on Linux |
| Machine prerequisites | refused |

A Linux build writes one extensionless installer per target (for example `Acme-Setup`).

## Linux machine scope

Machine installs run through a short-lived privileged worker: the installer plans without authority, requests administrator authorization through `pkexec` at execution time, and the worker re-verifies the package and executes inside `/opt`, `/var/opt`, `/var/lib/zup`, and the owned unit sources. User installs never elevate. `zup build` never needs authority. Zup never handles an administrator password.

A machine project may declare static `[[services]]`. Each becomes a systemd system unit owned at `/usr/local/lib/systemd/system/<unit>.service`. Installing, upgrading, repairing, or uninstalling never starts, stops, or restarts a service: Zup registers boot policy (`automatic` enables for boot, `manual` installs without enabling, `disabled` masks) and systemd decides when the process runs. The service binary must be a Zup-owned payload executable; anything else is refused.

`zup check` and `zup build` accept services on any host without systemd. The installing machine needs systemd 240+ only when services are present; `zup doctor` reports that readiness.

# Platforms and targets

A target profile binds a project source directory to a canonical target triple:

```toml
[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist/windows-x64" }
```

The profile name is project-local. The triple is the machine identity the build uses.

Declare one profile per payload/target pair. `--target` accepts profile names or triples:

```bash
zup build --target windows-x64
```

With no `--target`, commands select every declared profile where the command allows it.

## Windows

Supported triples: `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc`. Frontends: `gui`, `console`, `headless`. Universal and thin artifacts are Windows installers.

A Windows build embeds the package into the runtime template as PE resources — plain file I/O, no Windows API on the build host.

## Linux

| Capability | Status |
|---|---|
| Target | `x86_64-unknown-linux-gnu` |
| Frontends | `console`, `headless` (`gui` is refused) |
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

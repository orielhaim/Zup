# Platforms and targets

A target profile binds a project source directory to a canonical target triple.

```toml
[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist/windows-x64" }
```

The profile name (`windows-x64`) is project-local. The triple is the machine identity Zup uses for the build.

## Platform status

<div class="platform-state">
  <div>Windows</div><div>Installation backend ships today.</div>
  <div>macOS</div><div>Architecture and public model support a backend; no installation backend ships today.</div>
  <div>Linux</div><div>User- and machine-scope installation backend ships for `x86_64-unknown-linux-gnu` (console and headless).</div>
</div>

Zup may parse a target no backend answers for, but `doctor` and `build` refuse it because there is no backend for that target yet.

## Linux support

Linux is a normal build target, from either host:

```bash
zup check --target x86_64-unknown-linux-gnu
zup build --target x86_64-unknown-linux-gnu
```

A Linux build produces one self-contained, extensionless installer per target (for example `Acme-Setup`), composed by appending the normal package behind the Linux runtime template. The installer runs the same install, upgrade, repair, and uninstall lifecycle as every other target.

| Capability | Linux status |
|---|---|
| Target | `x86_64-unknown-linux-gnu` only |
| Frontends | console, headless |
| Scope | user, machine, either (either requires an explicit `--scope` choice) |
| Artifact | one self-contained installer per target |
| Signing | no platform-native signature; artifact digest and release identity carry authenticity (`zup sign verify --allow-unsigned` finalizes the measured bytes) |
| GUI installer | not supported |
| Machine scope elevation | `pkexec` at install time; `zup build` needs no authority |
| Machine payload | `/opt/<App>` for `${location.programs}`, `/var/opt` for shared data, `/var/lib/zup` for Zup state |
| Machine desktop integration, services, PATH entries, protocols, file associations, package-manager prerequisites | not supported |
| User desktop integration (`.desktop`, icons, MIME) | user scope only |
| systemd, D-Bus, package managers | not supported |
| Universal/dispatcher and thin artifacts | not supported; Windows-only |

## Linux machine scope

A machine-scope installer (`scope = "machine"`, or `either` with
`--scope machine`) performs its transaction through a short-lived
privileged worker:

```text
unprivileged installer plans and shows the operation
→ administrator authorization through `pkexec`
→ privileged worker re-verifies the package, reconstructs the plan,
  enforces the privileged path policy, and executes
→ ledger, journals, and maintenance generation under `/var/lib/zup`
```

User-scope installs never elevate. Machine installs request elevation
only when executed, never during `zup build` - including when
cross-built from Windows. `zup doctor` reports the `pkexec` elevation
capability for machine projects without ever prompting. Zup never
handles an administrator password; authentication belongs to polkit.

The worker performs typed installation operations only: static files,
executable intent, ledger, maintenance, locking, install, upgrade,
repair, uninstall, and recovery. It never launches the installed
application, never executes plugins or prerequisite installers, and
never writes outside `/opt`, `/var/opt`, and `/var/lib/zup`. An
`--install-dir` override stays inside the machine program tree.

A project can declare Windows and Linux target profiles side by side and builds them into separate native artifacts. Unsupported Linux configurations fail during `check`/`build` capability validation with a diagnostic naming the configuration.

## Multiple targets

Declare one profile per payload/target pair:

```toml
[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist/windows-x64" }

[build.targets.windows-arm64]
target = "aarch64-pc-windows-msvc"
source = { directory = "dist/windows-arm64" }
```

Commands accept a profile name or canonical triple with repeatable `--target`:

```bash
zup check --target windows-x64
zup build --target windows-x64
```

With no `--target`, project commands select every declared profile where the command allows it.

## Target-specific resources

Resource declarations can carry `targets`:

```toml
[[files]]
source = "helper-arm64.exe"
destination = "${install}"
targets = ["windows-arm64"]
```

Use this for a real target difference. Keep shared resources target-agnostic.

## Frontend per target

The project frontend defaults to `gui`. A target profile can override it when one target needs `console` or `headless` behavior.

The exact frontend fields belong in the [manifest reference](/reference/manifest#frontends).

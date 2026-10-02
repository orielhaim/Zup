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
  <div>Linux</div><div>Portable parts build and test there; no installation backend ships today.</div>
</div>

Zup may parse a non-Windows target, but `doctor` and `build` refuse it because there is no backend for that target yet.

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

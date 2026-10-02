# Frontends

`frontend` controls how the generated installer is operated. It does not change the application payload.

```toml
frontend = "gui"
```

Three frontends are available.

## GUI

```toml
frontend = "gui"
```

The normal desktop installer. GUI packages use a [preset](/presets/) for install and maintenance presentation.

`zup preview` is available for GUI targets because there is a window to present.

## Console

```toml
frontend = "console"
```

A console installer for terminal use. When attached to an interactive terminal it can prompt and show terminal progress.

Use this when the installer should be operated by a person in a shell rather than through a native window.

## Headless

```toml
frontend = "headless"
```

A non-interactive installer for automation, provisioning and remote execution. Headless mode does not prompt; callers provide an explicit lifecycle operation and options.

Use machine-readable output when a program consumes the result. See [Automation output](/reference/automation).

## Per-target override

Set a project default and override only the profiles that differ:

```toml
frontend = "gui"

[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist/windows-x64" }

[build.targets.windows-arm64]
target = "aarch64-pc-windows-msvc"
source = { directory = "dist/windows-arm64" }
frontend = "headless"
```

A GUI preset is irrelevant to console and headless targets. Keep preset-specific configuration attached to GUI applications rather than treating the preset as a second frontend selector.

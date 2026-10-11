# Frontends

`frontend` controls how the installer is operated. It does not change the payload.

```toml
frontend = "gui"
```

- `gui`: the desktop installer, presented by a [preset](/presets/). `zup preview` needs a `gui` target. Windows-only.
- `console`: terminal use, with prompts and progress on an interactive terminal.
- `headless`: no prompts; for automation, provisioning and remote execution. Pair with [automation output](/reference/automation).

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

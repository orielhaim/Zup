# Frontends

`frontend` controls how the installer is operated. It does not change the payload.

```toml
frontend = "gui"
```

- `gui`: the desktop installer, presented by a [preset](/presets/). `zup preview` needs a `gui` target. Linux requires a Wayland or X11 session.
- `console`: terminal use, with prompts and progress on an interactive terminal.
- `headless`: no prompts; for automation, provisioning and remote execution. Pair with [automation output](/reference/automation).

## Per-target override

Set a project default and override only the profiles that differ:

```toml
frontend = "gui"

[build.targets.windows-x64]
source = { directory = "dist/windows-x64" }

[build.targets.windows-arm64]
source = { directory = "dist/windows-arm64" }
frontend = "headless"
```

A GUI preset is irrelevant to console and headless targets. Keep preset-specific configuration attached to GUI applications rather than treating the preset as a second frontend selector.

## Terminal installation and maintenance

Console uses the same inline prompts on Windows and Linux. Start the installer without an operation to install or open maintenance. It asks only about available choices: scope, optional component groups, and an allowed installation directory. Required components stay selected. Review the resolved plan before confirming. Machine-wide changes still require native OS authorization.

Use arrow keys to navigate, Space to toggle components, and Enter to continue. Long component lists support filtering. Escape or Ctrl+C cancels a prompt. During execution, Ctrl+C requests cancellation at a safe boundary; wait for the installer to finish rollback.

`--plain` uses numbered choices and line prompts without terminal effects. Narrow terminals and `TERM=dumb` use this mode automatically. `NO_COLOR` disables colors. Redirecting any standard stream disables interactive prompts; Headless never prompts, including when started in a terminal.

The common operation interface is:

```sh
./Acme-Setup install --scope user --enable documentation
./Acme-Setup modify --scope user --disable documentation
./Acme-Setup upgrade --scope user
./Acme-Setup repair --scope user
./Acme-Setup uninstall --scope user
```

Use the `.exe` executable on Windows. An installer declaring `scope = "either"` requires `--scope` outside interactive mode. Fixed-scope installers do not ask. `--install-directory PATH` is accepted only when the manifest permits it. Repair uses the committed location and component selection. `repair --force-files` explicitly permits restoring edited managed files; ordinary repair preserves them or reports a conflict.

Thin runtimes collect component choices before acquiring selected content. Their launcher selects scope before acquiring the runtime. Acquisition still authenticates release metadata and verifies content before mutation. Windows maintenance may open a staged helper terminal so uninstall can remove the installed runtime after its original process exits. For automation that must observe the final process exit code, run the downloaded installer rather than its installed self-uninstall entry.

See [automation output](/reference/automation) for JSON and live JSONL results. `--yes` or `--non-interactive` also suppresses Console prompts. Neither flag replaces native authorization.

See [terminal installer measurements](/ship/terminal-size) for dependency choices, distribution sizes, and verification limits.

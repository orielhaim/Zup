# Check and preview

Use four commands before a release. They answer different questions.

| Command | Question |
| --- | --- |
| `zup check` | Is the project valid? |
| `zup doctor` | Can this machine build the selected targets now? |
| `zup plan` | What would the selected installation change? |
| `zup preview` | What will the installer window present? |

## Validate the project

```bash
zup check
```

Run this after manifest, payload or extension changes. It validates the project instead of waiting for a full build to discover a configuration error.

## Check the build host

```bash
zup doctor
```

`doctor` is read-only. It checks build readiness for selected targets, including whether a platform backend is available on the current host.

## Inspect the installation plan

```bash
zup plan --scope user
```

For components:

```bash
zup plan --enable docs --disable samples
```

Use `plan` to inspect owned files and system resources without installing them.

## Preview the installer

```bash
zup preview
```

Preview resolves the same preset and `[ui.settings]` the build uses, then runs
that preset against a simulated machine. It does not write application files,
PATH entries, shortcuts, services or uninstall state.

Use `zup preview` when authoring an application. Preset authors use
[`zup preset dev`](/presets/develop), which builds the preset source and supplies
a development scenario.

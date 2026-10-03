# Troubleshooting

Start with the command that owns the failing stage. Do not debug a build by guessing at installer internals.

## Manifest or payload errors

```bash
zup check
```

Use this first for unknown keys, invalid component/target references, missing files, invalid plugin components and preset-setting errors.

Keep the schema directive in `zup.toml` so the editor catches many of these before the CLI runs.

## Build host errors

```bash
zup doctor
```

Use `doctor` when the project is valid but cannot build on the current machine. A non-Windows installation target currently fails because no non-Windows backend ships yet.

## The plan is wrong

```bash
zup plan --scope user
```

Add `--enable` / `--disable` to reproduce component choices. Fix the manifest or plugin that declares the wrong resource; do not patch the generated installer.

## The window is wrong

Application author:

```bash
zup preview
```

Preset author:

```bash
zup preset dev
```

Use `preview` to test a project's selected preset and settings. Use `ui dev` to iterate on preset source.

## Preset settings are rejected

Inspect the package:

```bash
zup preset inspect path/to/preset.zupui
```

Then compare `[ui.settings]` with the preset's schema. Settings belong to the selected preset; they are not global Zup options.

## Plugin planning fails

Confirm the file is a WebAssembly component exporting the current Zup planner world:

```bash
wasm-tools component wit plugin.component.wasm
```

Then run `zup check`. If the plugin runs but returns the wrong resources, reduce it to one context-dependent resource and inspect `zup plan`.

## Release workflow is stale

```bash
zup ci github check
```

Regenerate and review the diff:

```bash
zup ci github generate --force
```

## Need exact parser behavior

Use the version on your machine:

```bash
zup <command> --help
zup schema
```

Those are authoritative for CLI flags and manifest shape.

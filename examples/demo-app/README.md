# Demo App

A Windows example with required and optional components, user or machine scope, shortcuts, a PATH entry, and installer branding. Payloads use immutable generations. Shortcuts and PATH registration refer to the selected generation. Read `zup.toml` for its declarations.

## Run

From the repository root, build the toolchain once:

```sh
cargo xtask toolchain build
```

From this directory:

```sh
zup check
zup preview
zup plan --scope user
zup build
zup artifact inspect "Demo App-Setup.exe"
```

Preview simulates installation without installing application files or changing system integration. Edit the manifest while it is open to test live updates; invalid edits leave the last valid preview running.

## Test the installer

Run `Demo App-Setup.exe`:

1. Choose user or machine scope. Machine scope requires elevation.
2. Toggle Documentation and Sample data and inspect the planned changes.
3. Install, then check the Start menu, desktop shortcut, and `demo --version` in a new command prompt.
4. Use Change to modify component selection.
5. Inspect the active generation, remove its `NOTES.txt`, and run Repair. Repair prepares a verified replacement generation.
6. Uninstall and verify that the managed files, shortcuts, and PATH entry are removed.

To test an upgrade, change the application version, rebuild with `zup build --force`, and run the new installer. This example does not configure an update repository.

## Branding

`[ui.settings]` configures the bundled preset's logo, accent, description, and initial window size. Unknown settings fail validation. A logo used by the installer UI is separate from an installed payload file.

See the [documentation](../../docs/guide/index.md) for manifest and preset authoring.

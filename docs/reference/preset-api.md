# Preset API

Preset authors depend on `zup-ui-sdk`.

## `Preset`

A preset defines:

- `NAME`
- `VERSION`
- associated `Settings` type
- optional required UI capabilities
- optional preset-owned assets
- `launch(PresetContext, &mut App)`

`zup_ui_sdk::run::<P>()` is the preset executable entry point.

## `PresetContext<T>`

Context provides the values a preset receives at launch:

- installer `UiSession`
- typed `PresetSettings<T>`
- application assets
- host description/capabilities

Use the supplied session. Do not create a replacement session for a hosted preset.

## `UiSession`

- `state()` returns observable installer state.
- `send(UiAction)` requests a typed action.

A preset renders from the current snapshot and asks the host to perform actions. It does not own lifecycle state.

## `PresetSettings<T>`

Settings are observable and read-only from the preset's point of view. Their schema comes from the preset's associated `Settings` type.

## Assets

- `ApplicationAssets` exposes application-provided UI assets.
- `AssetRef` identifies an asset setting.
- `Preset::assets()` exposes files compiled into the preset.

## `UiAction`

Current actions:

```text
SetScope
SetComponent
SetInstallDirectory
ResetInstallDirectory
Install
Update
Modify
Repair
RequestUninstall
ConfirmUninstall
DismissUninstall
Cancel
Retry
OpenLog
CopyDiagnostics
Launch
Close
```

## `UiSnapshot`

Snapshot areas:

```text
product
surface
state
operation
progress
plan
diagnostic
update
repair_drift
launch
```

See [State and actions](/presets/state-actions) for how these pieces fit together in a UI.

# Preset API

Preset authors depend on `zup-sdk` with the `preset` feature:

```toml
[dependencies]
zup-sdk = { version = "0.1.0", features = ["preset"] }
```

```rust
use zup_sdk::preset::prelude::*;
```

## `Preset`

| Item | Meaning |
| --- | --- |
| `NAME` | this preset's Cargo package name, from `env!("CARGO_PKG_NAME")` |
| `VERSION` | this preset's Cargo package version, from `env!("CARGO_PKG_VERSION")` |
| `type Settings` | the typed settings, satisfying the `Settings` bound |
| `required_capabilities()` | optional; what this preset cannot present without |
| `assets()` | optional; files compiled into the preset |
| `launch(PresetContext<Self::Settings>, &mut App)` | draw the installer |

`zup_sdk::preset::run::<P>()` is the whole of a preset's `main`. It describes the
preset when a build asks it to, and otherwise connects to its host, completes the
handshake, waits for the first snapshot, and hands the session to `launch`.

`NAME` and `VERSION` are read from Cargo rather than hard-coded because
`zup preset pack` cross-checks the document the preset describes itself with
against `cargo metadata`. A preset whose executable disagrees with its manifest
is refused rather than packaged under a name nobody can trace.

`required_capabilities()` names what the preset cannot present without. A host
refuses a preset that requires something it does not provide, before launching
it. The default is nothing:

```rust
fn required_capabilities() -> Capabilities {
    Capabilities::new([Capability::Components, Capability::PlanPreview])
}
```

The capabilities are `Components`, `Diagnostics`, `InstallDirectory`, `Launch`,
`Maintenance`, `PlanPreview` and `Updates`.

## `PresetContext<T>`

What a preset is given, once, at launch:

- `session()` - the connection to the installer
- `settings()` - `PresetSettings<T>`
- `assets()` - `ApplicationAssets`
- `capabilities()` - what the host says it provides

Use the supplied session. Do not open a second one.

## `Session`

- `state()` returns `Entity<SessionState>`, observable installer state
- `send(Action)` requests a typed action

`SessionState` offers `snapshot()`, `is_connected()` and `closed()`.

A preset renders from the current snapshot and asks the host to perform actions.
It does not own lifecycle state.

## `Action`

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

## `Snapshot`

```text
product        surface        state
operation      progress       plan
diagnostic     update         repair_drift
launch
```

`snapshot.surface.components()` gives the component options for the current
surface.

## `PresetSettings<T>`

Settings are observable and read-only from the preset's point of view. `read(cx)`
returns the current value and `observe(cx, ...)` subscribes to replacement. The
host owns them and may replace them while the session runs.

The schema comes from the preset's own `Settings` type:

```rust
#[zup_sdk::preset::settings]
pub struct Settings {
    pub hero: Option<String>,
    pub logo: Option<AssetRef>,
}
```

That attribute applies the default, the deserializer, and the JSON Schema Zup
packs into the `.zupui`. A preset with no settings uses `NoSettings`.

## Assets

- `ApplicationAssets` exposes application-provided assets; `path(&AssetRef)`
  resolves one, `names()` lists them
- `AssetRef` marks a settings field as a file the build resolves and hashes
- `Preset::assets()` exposes files compiled into the preset

The two are served through one asset source: application assets first, then the
preset's own, then the component library's icons. `img()` and the SVG renderer
are unchanged.

## GPUI

`zup_sdk::preset::prelude::*` re-exports `gpui` (the `gpui-kit` stack), plus
`App`, `AppContext`, `AsyncApp`, `Context`, `Entity`, `IntoElement`, `Render` and
`Subscription`.

See [State and actions](/presets/state-actions) for how these pieces fit together
in a window.

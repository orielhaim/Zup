# Settings and assets

Preset settings are typed Rust data. The settings type also generates the
schema that application manifests are validated against, so there is one
definition and nothing to keep in step.

## Typed settings

```rust
use zup_sdk::preset::prelude::*;

#[zup_sdk::preset::settings]
pub struct Settings {
    pub hero: Option<String>,
    pub accent: Option<String>,
    pub logo: Option<AssetRef>,
}
```

`#[zup_sdk::preset::settings]` generates the default, the deserializer, and the JSON Schema the build validates applications against — so a preset project depends on `zup-sdk` and nothing else. A preset with no settings uses `NoSettings`:

```rust
impl Preset for Preset {
    type Settings = NoSettings;
    // ...
}
```

The application supplies values under `[ui.settings]`:

```toml
[ui.settings]
hero = "Install Acme"
accent = "#695cff"
logo = "branding/logo.svg"
```

Keep every setting about presentation. Application install policy belongs in
`zup.toml` proper and reaches the preset through installer state, not through a
duplicate setting.

## Observe settings

`PresetContext::settings()` returns `PresetSettings<T>`, which derefs to the
GPUI entity holding the current value. Read it with `read(cx)` and observe it
with `observe(cx, ...)`:

```rust
let hero = self.settings.read(cx).hero.clone().unwrap_or_default();
```

Settings are observable because the host may replace them while the session runs. Do not copy them into unrelated long-lived state.

## Application assets

Use `AssetRef` for a file the application supplies to the preset:

```toml
[ui.settings]
logo = "branding/logo.svg"
```

The build resolves and verifies the source, then replaces the asset setting with its logical name (`logo` here). The host materializes the verified content. Use `ApplicationAssets::path(&asset_ref)` or pass `asset_ref.as_str()` to GPUI to load it. Each setting keeps its own name when several settings share the same content; project paths stay in the build.

A preset asset is a different thing from a payload file:

- a **preset asset** is read by the installer window;
- a **payload file** is installed onto the target machine.

If the same source file serves both purposes, declare each role explicitly.
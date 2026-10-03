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

`#[zup_sdk::preset::settings]` is an attribute rather than a derive because a
derive is appended to what it annotates and cannot add another derive to it. It
applies the default, the deserializer the host's configuration arrives through,
and the JSON Schema Zup puts in the package.

The consequence is that a preset project depends on `zup-sdk` and nothing else:
not on `serde`, not on `schemars`, and not on a GPUI version it has to track. A
preset with no settings at all uses `NoSettings`:

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

Settings are observable rather than a value because the host owns them and may
replace them while the session runs. A preset that copied them out at launch
would draw a configuration that no longer exists. Do not copy settings into
unrelated long-lived state unless the value is intentionally frozen.

## Application assets

Use `AssetRef` for a file the application supplies to the preset:

```toml
[ui.settings]
logo = "branding/logo.svg"
```

The type is what tells the build the value is a file rather than a string. The
build resolves the path, verifies it, hashes it, and stores it; the host
materializes it and tells the preset where the file is. The preset never learns a
project path - `ApplicationAssets::path(&asset_ref)` resolves a name to a
verified file, and the asset source serves it to `img()` and the SVG renderer
through GPUI's own interface.

A UI asset is a different thing from a payload file:

- a **preset asset** is read by the installer window;
- a **payload file** is installed onto the target machine.

If the same source file serves both purposes, declare each role explicitly
instead of relying on one path to imply the other.

## Preset-owned assets

`Preset::assets()` returns files compiled into the preset - its own icons, fonts
and illustrations. These are served after the application's assets and before the
component library's icons, so a name a preset ships is never shadowed by one the
library happens to share.
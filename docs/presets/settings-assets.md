# Settings and assets

Preset settings are typed Rust data. The settings type also generates the schema that application manifests are validated against.

## Typed settings

```rust
#[derive(Debug, Clone, Default, serde::Deserialize, schemars::JsonSchema)]
struct Settings {
    hero: Option<String>,
    accent: Option<String>,
    logo: Option<AssetRef>,
}
```

The application supplies values under `[ui.settings]`:

```toml
[ui.settings]
hero = "Install Acme"
accent = "#695cff"
logo = "branding/logo.svg"
```

Keep every setting about presentation. Application install policy belongs in `zup.toml` proper and reaches the preset through installer state, not through a duplicate setting.

## Observe settings

`PresetContext::settings()` returns `PresetSettings<T>`. Read it from GPUI state and observe it when a live development session can replace settings.

Do not copy settings into unrelated long-lived state unless the value is intentionally frozen.

## Application assets

Use `AssetRef` for files an application supplies to the preset. Zup resolves configured asset references and exposes the materialized application assets through `PresetContext`.

The generated preset project demonstrates a logo setting. A UI asset is different from a payload file:

- a **UI asset** is read by the installer window;
- a **payload file** is installed onto the target machine.

If the same source file serves both purposes, declare each role explicitly instead of relying on one path to imply the other.

## Preset-owned assets

A preset can also compile its own icons, fonts or illustrations through `Preset::assets()`. Use these for visual resources that belong to the preset itself rather than to the application using it.

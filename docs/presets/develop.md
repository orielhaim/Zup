# Development loop

Run a preset against a simulated installer:

```bash
zup preset dev
```

`zup preset dev` builds the preset project and opens it with development state. It is the preset-author equivalent of application-level `zup preview`.

## Development configuration

The generated `zup.preset.dev.toml` supplies preset settings and application assets:

```toml
[settings]
hero = "Install Acme"
accent = "#695cff"

[assets]
"branding/logo.svg" = "assets/logo.svg"
```

This file exists only for preset development. It is not an application manifest and is not packaged as the preset's configuration.

## Edit by cost

Three changes have different loops:

- Rust source changes require a preset rebuild.
- settings changes are revalidated and republished;
- asset changes can be rematerialized without recompiling Rust.

Keep layout iteration in the preset project. Keep application-specific values in `[ui.settings]` so one preset can serve many applications.

## Test application integration

After packaging the preset, use it from a real Zup project and run:

```bash
zup check
zup preview
```

That verifies package selection, the application's settings, and its actual component/scope model.

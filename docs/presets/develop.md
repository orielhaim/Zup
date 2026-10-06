# Development loop

Run a preset against a simulated installer:

```bash
zup preset dev
```

`zup preset dev` builds the preset project and opens it with development state.
It is the preset-author equivalent of application-level `zup preview`.

Pass `--project <dir>` to develop a preset somewhere else, and `--profile <p>` to
build with something other than `dev`.

## Development configuration

The generated `zup.preset.dev.toml` supplies preset settings and application assets:

```toml
[settings]
hero = "Install Acme"
accent = "#695cff"

[assets]
"branding/logo.svg" = "assets/logo.svg"
```

This file exists only for preset development. It is not an application manifest,
nothing in it is a preset format, and no build reads it.

## Edit by cost

Three changes have different loops:

| Change | What happens |
| --- | --- |
| A `.rs` file | Rebuild and replace the preset |
| `zup.preset.dev.toml` settings | Revalidate and republish |
| An asset file | Rematerialize, no recompile |

The cheapest loop wins, so editing an asset does not cost a rebuild. A build that
fails leaves the previous preset running rather than taking the window down.

Settings here are validated against the schema your own `Settings` type
generates. An invalid one is reported and the last valid settings stay in force,
so a half-finished edit cannot empty a working window.

## Keep application values out of the preset

Keep layout iteration in the preset project. Keep application-specific values in
`[ui.settings]` so one compiled preset serves many applications - see
[Settings and assets](./settings-assets).

## Test application integration

After packaging the preset, use it from a real Zup project and run:

```bash
zup check
zup preview
```

That verifies package selection, the application's settings, and its actual
component/scope model.
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

## Edit by cost

Three changes have different loops:

| Change | What happens |
| --- | --- |
| A `.rs` file | Rebuild and replace the preset |
| `zup.preset.dev.toml` settings | Revalidate and republish |
| An asset file | Rematerialize, no recompile |

A failed build leaves the previous preset running.

## Keep application values out of the preset

Keep application-specific values in `[ui.settings]` so one compiled preset serves many applications - see
[Settings and assets](./settings-assets).
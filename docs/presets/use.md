# Use a preset

Without `[ui].preset`, Zup uses the preset it ships.

To use another package:

```toml
[ui]
preset = "./ui/aurora.zupui"
```

The path is relative to the project.

## Configure it

A preset defines its own settings. Put values under `[ui.settings]`:

```toml
[ui]
preset = "./ui/aurora.zupui"

[ui.settings]
hero = "Install Acme"
accent = "#695cff"
logo = "branding/logo.svg"
```

Those key names come from the preset's schema, not from Zup. `zup check` validates them and names the failing path.

## Preview application changes

```bash
zup preview
```

An invalid setting reports the schema's refusal naming the setting, and the last valid settings stay in force.

Preset source development is a separate loop: [`zup preset dev`](./develop).
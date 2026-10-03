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

Those key names come from the preset's schema, not from Zup. `zup check`
validates them against the schema packaged with `aurora.zupui` and names the
failing path, so a typo is a diagnostic rather than a window that quietly ignores
the setting.

That separation is intentional: the manifest chooses a preset package and supplies
that package's settings. It does not describe screens, controls or layout.

## Preview application changes

```bash
zup preview
```

Preview resolves the same preset and `[ui.settings]` the build uses. Use it after
changing the preset package, `[ui.settings]`, or configured assets.

Editing a setting into something invalid reports the schema's own refusal,
naming the setting, and keeps showing the last settings that fitted.

Preset source development is a separate loop: [`zup preset dev`](./develop).

## Cross-platform packages

A `.zupui` can contain native binaries for multiple target triples. The
application references one package; Zup selects the matching binary for the
target being built.

See [Package a preset](./package).
# Use a preset

Without `[ui].preset`, Zup uses the preset it ships.

To use another package:

```toml
[ui]
preset = "./ui/aurora.zupui"
```

The path is relative to the project.

## Configure it

A preset defines its own settings schema. Put values under `[ui.settings]`:

```toml
[ui]
preset = "./ui/aurora.zupui"

[ui.settings]
hero = "Install Acme"
accent = "#695cff"
logo = "branding/logo.svg"
```

`zup check` validates these values against the schema packaged with `aurora.zupui`. There is no global Zup vocabulary for preset styling.

That separation is intentional: the manifest chooses a UI package and supplies that package's settings. It does not describe screens, controls or layout.

## Preview application changes

```bash
zup preview
```

Preview watches the application-facing preset inputs. Use it after changing the preset package, `[ui.settings]`, or configured UI assets.

Preset source development is a separate loop: [`zup ui dev`](./develop).

## Cross-platform packages

A `.zupui` can contain native binaries for multiple target triples. The application references one package; Zup selects the matching binary for the target being built.

See [Package a preset](./package).

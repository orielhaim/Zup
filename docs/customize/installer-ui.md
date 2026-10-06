# Installer UI

`[ui]` chooses the preset that draws the installer window and fills in that
preset's settings. Those two keys are the whole of it.

```toml
[ui]
preset = "./acme-brand.zupui"

[ui.settings]
accent = "#2563eb"
appearance = "dark"
logo = "branding/logo.svg"
```

Omitting `preset` is not a fallback to nothing. It selects the preset Zup ships,
which is that preset's own UI selection semantics.

## What the shipped preset offers

The preset that ships with Zup exposes three settings.

| Setting | Type | Effect |
| --- | --- | --- |
| `logo` | a project-relative path | Your application's mark, shown beside its name and in the title bar |
| `accent` | `#rrggbb` | The colour of the main button, selections and progress |
| `appearance` | `system`, `light` or `dark` | Which theme to use. `system` follows the person's Windows setting |

In the preset's own settings type, that is three fields:

```rust
#[zup_sdk::preset::settings]
pub struct Settings {
    pub logo: Option<AssetRef>,
    pub accent: Option<Accent>,
    pub appearance: Appearance,
}
```

`AssetRef` is what marks a setting as a file, so the build resolves it.

```toml
[ui]
[ui.settings]
logo = "branding/logo.svg"
accent = "#0f766e"
appearance = "system"
```

The accent is not taken on trust. Zup adjusts it until the text on the button
and the button against the background both clear the contrast thresholds, and
picks black or white ink to match. A colour that cannot reach the thresholds is
not used as given.

::: tip These are the preset's settings, not Zup's
Zup itself has no theme key, no accent key and no logo key. It has `[ui].preset`
and `[ui].settings`. The names and types come from the selected preset's schema,
so with a different preset these keys either do not exist or are ignored.
:::

## Assets

A setting whose schema is marked as an asset is a path to a file the **window
itself reads**. Zup resolves it, hashes it, and hands the preset a name and the
verified bytes - never a path into your project.

That is a different thing from a file your payload ships. `branding/logo.svg`
under `[ui.settings]` is drawn in the installer's chrome. The same path under
`[[files]]` is installed into the application directory, listed in the plan,
restored by a repair, and removed by an uninstall. Use whichever one you meant.

Asset paths are project-relative, may not be absolute, and may not contain `..`.
The file may be at most 32 MiB, and a `[ui.settings]` document as a whole at most
256 KiB.

## Using a different preset

Name a package inside your project:

```toml
[ui]
preset = "./acme-brand.zupui"
```

Zup verifies the package before reading anything out of it, checks that the preset
protocol matches, that a binary exists for the target being built, and that your
settings satisfy the schema the package carries. A failure at any of those is a
failure at `zup check`, not at the end of a build.

The diagnostic names the setting:

```text
acme-brand does not accept the configured settings: ui.settings.accent: string is not valid under any of the given schemas
```

The bundled preset is inspected like any other:

```bash
zup preset inspect ./acme-brand.zupui
```

```text
preset        acme-brand 1.0.0
package       schema 1
preset protocol   1
capabilities  none required
settings      accent, appearance, logo (others permitted)
```

`(others permitted)` means the preset's schema does not set
`additionalProperties: false`, so an unrecognized key validates and is ignored.
It is the honest detail: the key is accepted, not honoured.

## Previewing

```bash
zup preview
```

The window you get is the one a build composes - the same package, read by the
same reader, selected by the same resolver, with the settings your manifest
names. Edit `[ui.settings]` while it is open and it re-renders without a
restart. Edit a setting into something invalid and it reports the schema's
refusal, naming the setting, and keeps showing the last settings that fitted.

## Settings the window offers

A preset reports what it can present, and Zup derives that from your manifest
rather than from the preset's preference:

| Capability | Offered when |
| --- | --- |
| Components | You declared any |
| Install directory | `allow_directory_override` is on |
| Maintenance | The application is already installed |
| Updates | You configured `[updates]` |
| Launch | You declared a launcher |
| Plan preview | Always |
| Diagnostics | Always |

Next: [writing a preset](/customize/presets).

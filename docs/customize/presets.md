# Writing a preset

A preset is the program that draws the installer window. Zup ships one; this page
is for when the shipped one is not the window you want.

A preset is a per-target native binary packed into a `.zupui` file. It is a
serious commitment: you build it for every target you support.

## Create one

```bash
zup ui init acme-brand
cd acme-brand
```

```text
acme-brand/
  Cargo.toml
  src/main.rs
  zup.ui.dev.toml
  .gitignore
```

The generated project is the smallest thing that compiles. It depends on
`zup-ui-sdk` and draws three things: the product name, one line of settings, and
a button per component.

## Develop it

```bash
zup ui dev
```

Builds and runs the preset against a simulated installer, and watches three
things:

| Change | What happens |
| --- | --- |
| A `.rs` file | Rebuild and replace the preset |
| `zup.ui.dev.toml` | Resend the configuration |
| An asset file | Rematerialize it |

Cheapest first, so editing an asset does not cost a rebuild. The session shows
the compiler's diagnostics, and a build that fails leaves the previous preset
running rather than taking the window down.

`zup.ui.dev.toml` is not a preset format and is never read by a build. It is a
development convenience:

```toml
[settings]
hero = "Install Acme"
accent = "#695cff"

[assets]
"branding/logo.svg" = "assets/logo.svg"
```

Settings here are validated against your own `Settings` type's schema. An
invalid one is reported and the last valid settings stay in force.

## The shape of a preset

There is no layout abstraction, no screen abstraction and no widget vocabulary.
You have GPUI, `gpui-kit`, and ordinary Rust, and any of them could express a
layout the others could not.

```rust
use zup_ui_sdk::prelude::*;

#[derive(Debug, Clone, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct Settings {
    pub hero: Option<String>,
    pub logo: Option<AssetRef>,
    pub accent: Option<String>,
}

struct AcmeBrand;

impl zup_ui_sdk::Preset for AcmeBrand {
    const NAME: &'static str = env!("CARGO_PKG_NAME");
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");
    type Settings = Settings;

    fn launch(context: PresetContext<Self::Settings>, cx: &mut App) {
        gpui::open_window(gpui::WindowOptions::default(), cx, |window, cx| {
            cx.new(|_window, cx| View {
                session: UiSession::open(cx, |action| { /* forward to the host */ }),
                settings: context.settings().clone(),
                _settings: context.settings().observe(cx, |_, cx| cx.notify()),
                ..View::new(cx)
            })
        });
    }
}

fn main() {
    if let Err(error) = zup_ui_sdk::run::<AcmeBrand>() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
```

## The two things a preset can reach

A preset is not a UI toolkit. It gets the state Zup publishes, and it can ask
the host to do things. It has no access to a plan, a transaction, an elevation
decision or a package - that boundary is what makes a third-party preset safe
to ship, and asking for one is not an omission.

### Observe

```rust
let snapshot = session.state().read(cx).snapshot();
```

`UiSnapshot` carries the product identity, the surface, the current state, the
plan, the running operation and its progress, any diagnostic, and the update
status. It is a snapshot: observing it redraws your view when it changes, and
you do not poll.

`PresetSettings<T>` works the same way, and `ApplicationAssets::path(&asset_ref)`
resolves an asset the application declared.

### Ask

```rust
session.send(UiAction::SetComponent { component: "docs".into(), selected: true });
session.send(UiAction::Install);
```

That is the complete vocabulary:

| | |
| --- | --- |
| Scope | `SetScope`, `SetInstallDirectory`, `ResetInstallDirectory` |
| Components | `SetComponent`, `Modify` |
| Lifecycle | `Install`, `Update`, `Repair`, `Uninstall` (as `RepairRequestUninstall`) |
| Confirmation | `ConfirmUninstall`, `DismissUninstall` |
| Session | `Cancel`, `Retry`, `Launch`, `Close` |
| Support | `OpenLog`, `CopyDiagnostics` |

Actions are intent, not interaction. There is no "button was pressed", no widget
id and no generic command channel. The host validates every action against the
state it owns, so sending `Install` twice does not start two installations.

## Settings

Declare settings as an ordinary `Deserialize` + `JsonSchema` struct. Zup
generates the schema, packs it into the `.zupui`, validates `[ui.settings]`
against it, and reports the failing path.

Mark a path-typed setting as an asset so Zup resolves, hashes and hands over
its bytes:

```rust
#[schemars(extend("x-zup-asset" = true))]
pub logo: Option<AssetRef>,
```

Your schema must be self-contained. A `$ref` to anything but a local `#/...`
path is refused, so a preset cannot make a build fetch a schema from the
network.

## Pack it

```bash
zup ui pack --build x86_64-pc-windows-msvc
```

Writes `acme-brand-1.0.0.zupui` here. `--build` builds a target with Cargo and
includes the result; `--binary TRIPLE=FILE` includes one you built elsewhere. You
need at least one. A binary for the host triple is built for the packaging step
whether or not it is included.

The package is deduplicated by digest, so two targets with identical bytes are
stored once. Reading a package never trusts a declared size or digest - it
decompresses, measures and hashes.

Name it in your application:

```toml
[ui]
preset = "./acme-brand-1.0.0.zupui"
```

## Check a package

```bash
zup ui inspect acme-brand-1.0.0.zupui
```

Reports the name, version, schema, UI protocol, required capabilities, the
settings it accepts, and every target with its size and digest - without running
anything.

## Limits

| Limit | Value |
| --- | --- |
| Targets per package | 64 |
| Binary, compressed | 256 MiB |
| Total binaries | 1 GiB |
| Settings document | 256 KiB |
| Assets | 256 |
| Asset path | 1024 bytes |
| Asset file | 32 MiB |

::: warning A preset is native code and is not sandboxed
It is a child process the host launches, with the same authority as the
installer itself. The transport between them is not a privilege boundary. Only
install a `.zupui` you would be willing to run.
:::

Next: [signing](/ship/signing).

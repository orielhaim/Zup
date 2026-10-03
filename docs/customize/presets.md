# Writing a preset

A preset is the program that draws the installer window. Zup ships one; this page
is for when the shipped one is not the window you want.

A preset is a per-target native binary packed into a `.zupui` file. It is a
serious commitment: you build it for every target you support.

## Create one

```bash
zup preset init acme-brand
cd acme-brand
```

```text
acme-brand/
  Cargo.toml
  src/main.rs
  zup.preset.dev.toml
  .gitignore
```

The manifest lists one dependency, `zup-sdk`, and the generated source draws
three things: the product name, one line of settings, and a button per component.
There is no `preset.toml`, no layout file, no screen abstraction and no component
wrappers - every one of those is something to learn before you could draw
anything, and GPUI already exists.

## Develop it

```bash
zup preset dev
```

Builds and runs the preset against a simulated installer, and watches three
things:

| Change | What happens |
| --- | --- |
| A `.rs` file | Rebuild and replace the preset |
| `zup.preset.dev.toml` | Resend the configuration |
| An asset file | Rematerialize it |

Cheapest first, so editing an asset does not cost a rebuild. The session shows
the compiler's diagnostics, and a build that fails leaves the previous preset
running rather than taking the window down.

`zup.preset.dev.toml` is not a preset format and is never read by a build. It is a
development convenience:

```toml
[settings]
hero = "Install Acme"
accent = "#695cff"

[assets]
"branding/logo.svg" = "assets/logo.svg"
```

Settings here are validated against the schema your own `Settings` type
generates. An invalid one is reported and the last valid settings stay in force.

## The shape of a preset

There is no layout abstraction, no screen abstraction and no widget vocabulary.
You have GPUI, the `gpui-kit` component library, and ordinary Rust, and any of
them could express a layout the others could not.

```rust
use zup_sdk::preset::prelude::*;

#[zup_sdk::preset::settings]
pub struct Settings {
    pub hero: Option<String>,
    pub logo: Option<AssetRef>,
    pub accent: Option<String>,
}

struct AcmeBrand;

impl Preset for AcmeBrand {
    const NAME: &'static str = env!("CARGO_PKG_NAME");
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");
    type Settings = Settings;

    fn launch(context: PresetContext<Self::Settings>, cx: &mut App) {
        let session = context.session().clone();
        let settings = context.settings().clone();
        let state = session.state();

        gpui::open_window(gpui::WindowOptions::default(), cx, move |_window, cx| {
            let view = cx.new(|_| View {
                session: session.clone(),
                state: state.clone(),
                settings: settings.clone(),
                _state: Subscription::new(|| {}),
                _settings: Subscription::new(|| {}),
            });
            let refreshed = view.clone();
            view.update(cx, |view, cx| {
                view._state = cx.observe(&state, |_, _, cx| cx.notify());
                view._settings = settings.observe(cx, move |_, cx| {
                    refreshed.update(cx, |_, cx| cx.notify());
                });
            });
            view
        });
    }
}

fn main() {
    if let Err(error) = zup_sdk::preset::run::<AcmeBrand>() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
```

`zup_sdk::preset::prelude::*` brings the GPUI stack with it, so the manifest
needs nothing else and there is no GPUI version to keep in step with Zup's.

Two naming rules the generated source depends on. The preset struct cannot be
called `Preset` - a struct and a trait share a namespace, so `impl Preset for
Preset` names the struct and does not compile. And `Window` is not in the
prelude, so a `Render` signature writes `gpui::Window`; an import inside
`render` does not help, because the signature is outside its body.

## The two things a preset can reach

A preset is not a UI toolkit. It gets the state Zup publishes, and it can ask
the host to do things. It has no access to a plan, a transaction, an elevation
decision or a package - that boundary is what makes a third-party preset safe to
ship, and asking for one is not an omission.

### Observe

```rust
let snapshot = session.state().read(cx).snapshot();
```

`Snapshot` carries the product identity, the surface, the current state, the
plan, the running operation and its progress, any diagnostic, and the update
status. It is a snapshot: observing it redraws your view when it changes, and you
do not poll.

`PresetSettings<T>` works the same way, and `ApplicationAssets::path(&asset_ref)`
resolves an asset the application declared.

### Ask

```rust
session.send(Action::SetComponent {
    component: "docs".into(),
    selected: true,
});
session.send(Action::Install);
```

That is the complete vocabulary:

| | |
| --- | --- |
| Scope | `SetScope`, `SetInstallDirectory`, `ResetInstallDirectory` |
| Components | `SetComponent`, `Modify` |
| Lifecycle | `Install`, `Update`, `Repair`, `RequestUninstall` |
| Confirmation | `ConfirmUninstall`, `DismissUninstall` |
| Session | `Cancel`, `Retry`, `Launch`, `Close` |
| Support | `OpenLog`, `CopyDiagnostics` |

Actions are intent, not interaction. There is no "button was pressed", no widget
id and no generic command channel. The host validates every action against the
state it owns, so sending `Install` twice does not start two installations.

## Settings

`#[zup_sdk::preset::settings]` applies the derives Zup needs. Zup generates the
schema from the resulting type, packs it into the `.zupui`, validates
`[ui.settings]` against it, and reports the failing path.

Because the schema comes from your types, a preset project depends on `zup-sdk`
and neither `serde` nor `schemars`. There is one version of the schema generator
in the graph and it is the one your preset deserializes with.

Use `AssetRef` for a path-typed setting:

```rust
pub logo: Option<AssetRef>,
```

The type carries the marker the build reads to know a setting is a file, so Zup
resolves, hashes and hands over its bytes without any extra annotation.

Your schema must be self-contained. A `$ref` to anything but a local `#/...`
path is refused, so a preset cannot make a build fetch a schema from the
network.

## What a preset may require

`Preset::required_capabilities()` names what the preset cannot present without.
A host refuses a preset that requires something it does not provide, before the
preset is launched, so a missing capability is a clear message rather than a dead
control:

```rust
fn required_capabilities() -> Capabilities {
    Capabilities::new([Capability::Components, Capability::PlanPreview])
}
```

The default is nothing, which is right for a preset that can present whatever a
host has.

## Pack it

```bash
zup preset pack --build x86_64-pc-windows-msvc
```

Writes `acme-brand-1.0.0.zupui` here. `--build` builds a target with Cargo and
includes the result; `--binary TRIPLE=FILE` includes one you built elsewhere. You
need at least one. A binary for the host triple is built for the packaging step
whether or not it is included, because that is the copy asked to describe itself.

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
zup preset inspect acme-brand-1.0.0.zupui
```

Reports the name, version, schema, preset protocol, required capabilities, the
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
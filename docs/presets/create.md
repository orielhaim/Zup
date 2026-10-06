# Create a preset

Generate a preset project:

```bash
zup preset init aurora
cd aurora
```

That writes four files:

```text
aurora/
  Cargo.toml
  src/main.rs
  zup.preset.dev.toml
  .gitignore
```

The manifest lists one dependency:

```toml
[dependencies]
zup-sdk = { version = "0.1.0", features = ["preset"] }
```

## The generated preset

`src/main.rs` is about a hundred lines and is meant to be read once. Its shape:

```rust
use zup_sdk::preset::prelude::*;

/// What an application may configure about how this preset looks.
///
/// `#[settings]` applies the derives Zup needs - a default, the deserializer,
/// and the JSON Schema that validates an application's settings - so this
/// project depends on the SDK alone.
#[zup_sdk::preset::settings]
pub struct Settings {
    pub hero: Option<String>,
    pub logo: Option<AssetRef>,
    pub accent: Option<String>,
}

struct Aurora;

impl Preset for Aurora {
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
            // Re-render when the installer publishes state, and again when the
            // application changes what it configured. Both are ordinary entities.
            let refreshed = view.clone();
            view.update(cx, |view, cx| {
                view._state = cx.observe(&state, |_, _, cx| cx.notify());
                view._settings = settings.observe(cx, move |_, cx| {
                    refreshed.update(cx, |_, cx| cx.notify());
                });
            });
            view
        })
        .expect("open the installer window");
    }
}

struct View {
    session: Session,
    state: Entity<SessionState>,
    settings: PresetSettings<Settings>,
    _state: Subscription,
    _settings: Subscription,
}

impl gpui::Render for View {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        // ...
    }
}

fn main() {
    if let Err(error) = zup_sdk::preset::run::<Aurora>() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
```

The view is ordinary GPUI. It draws the product name, the `hero` setting, a
button per component, and an Install button that sends `Action::Install`.

::: warning Give the preset struct its own name
It cannot be called `Preset`. A struct and a trait occupy the same namespace, so
`impl Preset for Preset` resolves to the struct and does not compile. Name it
after the product - `Aurora`, `AcmeBrand` - and import the trait from the
prelude.
:::

::: tip `Window` is one path away
The prelude exports the GPUI names a preset's own signatures need. `Window` is
not among them, so write `gpui::Window` in a `Render` signature. An import
inside `render` does not help, because the signature is outside its body.
:::

## The parts

| | |
| --- | --- |
| `Preset` | the trait you implement: identity, settings type, `launch` |
| `PresetContext<S>` | the session, the settings, the assets, the host's description |
| `Session` | observable installer state in, typed `Action` out |
| `PresetSettings<S>` | the application's configuration, observable and read-only |
| `AssetRef` | a settings field that names a file the application provides |
| `zup_sdk::preset::run::<P>()` | the whole of a preset's `main` |

## GPUI comes from the SDK

`zup_sdk::preset::prelude::*` brings the GPUI stack with it - `gpui`, `App`,
`Entity`, `Render`, and the `gpui-kit` component library. There is nothing else
to add to the manifest and no Zup version to keep in step, because Zup owns the
GPUI version the preset builds against.

There is no Zup layout abstraction, no screen DSL and no markup. A preset draws
with GPUI, so it can draw anything GPUI can draw. Split views into modules, keep
state ownership explicit, and test presentation logic as ordinary Rust.

## Next

Run it with the [development loop](./develop), then read
[Settings and assets](./settings-assets) and [State and actions](./state-actions).
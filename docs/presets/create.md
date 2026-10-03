# Create a preset

Generate a preset project:

```bash
zup preset init aurora
cd aurora
```

The project contains ordinary Rust source plus `zup.preset.dev.toml` for development data.

## Minimal preset

A preset implements `Preset` and launches ordinary GPUI UI:

```rust
use zup_ui_sdk::gpui::{ParentElement, Window, div};
use zup_ui_sdk::prelude::*;

struct Aurora;
struct View;

impl Preset for Aurora {
    const NAME: &'static str = env!("CARGO_PKG_NAME");
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");
    type Settings = NoSettings;

    fn launch(_context: PresetContext<Self::Settings>, cx: &mut App) {
        gpui::open_window(gpui::WindowOptions::default(), cx, |_window, cx| {
            cx.new(|_| View)
        })
        .expect("open installer window");
    }
}

impl Render for View {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child("Installer")
    }
}

fn main() {
    if let Err(error) = zup_ui_sdk::run::<Aurora>() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
```

`PresetContext` provides the installer session, typed settings and application assets. The minimal view above ignores them; a real preset reads them from that context. Do not create a second `UiSession`.

## UI stack

`zup-ui-sdk` re-exports the GPUI stack used by Zup. The generated project also uses `gpui-kit` components. There is no Zup markup language or screen DSL.

That means normal Rust rules apply: split views into modules, keep state ownership explicit, and test presentation logic as code.

## Next

Run the project with [Development loop](./develop), then read [State and actions](./state-actions) before wiring lifecycle controls.

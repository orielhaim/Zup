//! `zup ui init`: a preset project, from nothing.
//!
//! The generated project is a normal Cargo project that depends on the SDK, and
//! it is small enough to read in one sitting. There is no `preset.toml`, no
//! layout file, no screen abstraction and no component wrappers, because every
//! one of those is a thing a preset author would have to learn before drawing
//! anything, and GPUI already exists.
//!
//! The two things that are not obvious are generated anyway, because getting
//! them wrong is expensive and nobody discovers it until a build takes four
//! minutes: the dependency versions, which must resolve against the published
//! SDK rather than a path, and the development profile, which makes the GPUI
//! stack compile once instead of every time.

use std::path::Path;

use crate::failure;

/// A name that is usable as a Cargo package and a directory.
fn package_name(raw: &str) -> Result<String, String> {
    let name: String = raw
        .trim()
        .to_owned()
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect();
    let name = name.trim_matches('-').to_owned();
    if name.is_empty() {
        return Err(format!("`{raw}` names nothing a package could be called"));
    }
    if name.starts_with(|character: char| character.is_ascii_digit()) {
        return Err(format!(
            "a package cannot be called `{name}`; it starts with a digit"
        ));
    }
    Ok(name)
}

/// Create a preset project named `name` in `parent`.
pub fn init(name: &str, parent: &Path) -> miette::Result<()> {
    let name = package_name(name).map_err(|error| failure::error("zup.ui.init_name", error))?;
    let root = parent.join(&name);
    if root.exists() {
        return Err(failure::error(
            "zup.ui.init_exists",
            format!("`{}` already exists", root.display()),
        ));
    }
    std::fs::create_dir_all(root.join("src")).map_err(|error| {
        failure::error("zup.ui.init_write", format!("{}: {error}", root.display()))
    })?;

    for (relative, contents) in [
        ("Cargo.toml", manifest(&name)),
        ("src/main.rs", MAIN.to_owned()),
        ("zup.ui.dev.toml", DEVELOPMENT.to_owned()),
        (".gitignore", GITIGNORE.to_owned()),
    ] {
        write(&root.join(relative), &contents)?;
    }
    Ok(())
}

fn write(path: &Path, contents: &str) -> miette::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            failure::error(
                "zup.ui.init_write",
                format!("{}: {error}", parent.display()),
            )
        })?;
    }
    std::fs::write(path, contents).map_err(|error| {
        failure::error("zup.ui.init_write", format!("{}: {error}", path.display()))
    })
}

fn manifest(name: &str) -> String {
    format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"
description = "A zup installer preset"
publish = false

[dependencies]
zup-ui-sdk = {{ version = "0.1.0" }}
gpui-kit = "0.7"
serde = {{ version = "1", features = ["derive"] }}
schemars = "1"

# GPUI is a very large dependency tree, and compiling it at `opt-level = 0` is
# both slow and, for a text and layout engine, surprisingly slow at run time.
# These are the packages that dominate the cost; everything else stays at the
# development default so a change to this preset's own code is compiled in
# seconds. Names are checked against the graph by `cargo build`, so a stack that
# renames one produces a warning rather than a silently unoptimised preset.
[profile.dev.package.gpui-pre]
opt-level = 2
[profile.dev.package.gpui-pre-platform]
opt-level = 2
[profile.dev.package.gpui-pre-shared-string]
opt-level = 2
[profile.dev.package.gpui-pre-scheduler]
opt-level = 2
[profile.dev.package.gpui-pre-refineable]
opt-level = 2
[profile.dev.package.gpui-pre-derive-refineable]
opt-level = 2
[profile.dev.package.gpui-pre-macros]
opt-level = 2
[profile.dev.package.gpui-pre-util]
opt-level = 2
[profile.dev.package.gpui-pre-util-macros]
opt-level = 2
[profile.dev.package.gpui-pre-derive-macro]
opt-level = 2
[profile.dev.package.gpui-base]
opt-level = 2
[profile.dev.package.gpui-component]
opt-level = 2
[profile.dev.package.gpui-kit-assets]
opt-level = 2
[profile.dev.package.taffy]
opt-level = 2
[profile.dev.package.smol_str]
opt-level = 2
[profile.dev.package.cosmic-text]
opt-level = 2
[profile.dev.package.swash]
opt-level = 2
[profile.dev.package.fontdue]
opt-level = 2
[profile.dev.package.rustybuzz]
opt-level = 2
[profile.dev.package.resvg]
opt-level = 2
[profile.dev.package.usvg]
opt-level = 2
[profile.dev.package.tiny-skia]
opt-level = 2
[profile.dev.package.image]
opt-level = 2
[profile.dev.package.zune-jpeg]
opt-level = 2
[profile.dev.package.png]
opt-level = 2
[profile.dev.package.tokio]
opt-level = 2
[profile.dev.package.smol]
opt-level = 2
"#
    )
}

const MAIN: &str = r#"use zup_ui_sdk::prelude::*;
use zup_ui_sdk::prelude::gpui::Window;

/// What an application may configure about how this preset looks.
///
/// Every field is optional and every one has a default, because an application
/// that configures nothing has to work. The schema this generates is what
/// validates an application's `[ui.settings]`, so a field added here is a field
/// an application can set without a second definition anywhere.
#[derive(Debug, Clone, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct Settings {
    /// A line above the product name.
    pub hero: Option<String>,
    /// A file this application provides, named relative to its project.
    pub logo: Option<AssetRef>,
    /// A `RRGGBB` accent colour.
    pub accent: Option<String>,
}

struct Preset;

impl zup_ui_sdk::Preset for Preset {
    const NAME: &'static str = env!("CARGO_PKG_NAME");
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    type Settings = Settings;

    fn launch(context: PresetContext<Self::Settings>, cx: &mut App) {
        let session = context.session().clone();
        let settings = context.settings().clone();
        let state = session.state();

        gpui::open_window(gpui::WindowOptions::default(), cx, move |window, cx| {
            let view = cx.new(|_| View {
                session: session.clone(),
                state: state.clone(),
                settings: settings.clone(),
                _state: Subscription::new(|| {}),
                _settings: Subscription::new(|| {}),
            });
            // The window re-renders when the installer state changes, and again
            // when the application changes what it configured. Both are ordinary
            // entities, which is what makes them ordinary to observe.
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

/// The window.
///
/// Nothing in here is a Zup concept: a preset is a GPUI program that draws what
/// the host published and asks for what it wants.
struct View {
    session: UiSession,
    state: Entity<SessionState>,
    settings: PresetSettings<Settings>,
    _state: Subscription,
    _settings: Subscription,
}

impl Render for View {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui::base::{Disableable, StyledExt};
        use gpui::component::button::Button;
        use gpui::component::{ActiveTheme, Theme};
        use gpui::{FontWeight, ParentElement, Styled, div, px};

        let theme: &Theme = cx.theme();
        let body = div()
            .v_flex()
            .gap_4()
            .p_6()
            .bg(theme.colors.background)
            .text_color(theme.colors.foreground);

        let Some(snapshot) = self.state.read(cx).snapshot() else {
            return body.child("Waiting for the installer…");
        };

        let mut column = body
            .child(
                div()
                    .text_size(px(21.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(snapshot.product.name.clone()),
            )
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(theme.colors.muted_foreground)
                    .child(self.settings.read(cx).hero.clone().unwrap_or_default()),
            );

        for component in snapshot.surface.components() {
            let action = UiAction::SetComponent {
                component: component.id.clone(),
                selected: !component.selected,
            };
            let session = self.session.clone();
            column = column.child(
                Button::new(component.id.to_string())
                    .label(format!(
                        "{} {}",
                        if component.selected { "[x]" } else { "[ ]" },
                        component.name
                    ))
                    .disabled(component.required)
                    .on_click(move |_, _, _| session.send(action.clone())),
            );
        }

        let session = self.session.clone();
        column.child(
            Button::new("install")
                .label("Install")
                .on_click(move |_, _, _| session.send(UiAction::Install)),
        )
    }
}

fn main() {
    if let Err(error) = zup_ui_sdk::run::<Preset>() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
"#;

const DEVELOPMENT: &str = r##"# The application `zup ui dev` presents.
#
# Settings are validated against the schema your `Settings` type generates, and
# an invalid one leaves the last valid settings in force, so a typo here cannot
# empty a window that is working. Nothing in this file is a preset format and
# none of it is read by a build.
[settings]
hero = "Install Acme"
accent = "#695cff"

# Application-provided assets, by the name the settings above refer to. Editing
# one of these files updates the running preset; it does not recompile anything.
[assets]
"branding/logo.svg" = "assets/logo.svg"
"##;

const GITIGNORE: &str = r#"/target
# A development environment's own state: run executables and the files it
# materialized for the simulated application. Disposable by construction.
/.zup
"#;

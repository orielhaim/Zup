//! `zup preset init`: a preset project, from nothing.
//!
//! The generated project depends on `zup-sdk` and nothing else. That is the
//! whole claim this file has to keep true: a preset author should not have to
//! know that `serde`, `schemars` and `gpui-kit` are how settings and windows are
//! built, or that the versions of those have to line up with Zup's.
//!
//! It is also small on purpose. There is no `preset.toml`, no layout file, no
//! screen abstraction and no component wrappers, because every one of those is a
//! thing a preset author would have to learn before drawing anything, and GPUI
//! already exists.
//!
//! The one thing that is not obvious is generated anyway, because getting it
//! wrong costs four minutes per build: the development profile, which makes the
//! GPUI stack compile once instead of every time.

use std::path::{Path, PathBuf};

use crate::failure;

/// A generated project: a name, a directory of files, and the diagnostic code
/// every failure writing one is reported under.
///
/// Shared by both generators because a generated project is a generated project:
/// they differ in the files they write, not in how they name one or how a
/// failure to write one is reported.
pub struct Generator {
    name: String,
    root: PathBuf,
    write_code: &'static str,
}

impl Generator {
    /// A project named `raw` in `parent`, reported under `code`.
    pub fn new(raw: &str, parent: &Path, code: &'static str) -> Result<Self, String> {
        let name = package_name(raw)?;
        let root = parent.join(&name);
        if root.exists() {
            return Err(format!("`{}` already exists", root.display()));
        }
        Ok(Self {
            name,
            root,
            write_code: code,
        })
    }

    /// The name it was asked for, normalised into a package name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Where the project will be.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Write the project, failing under this generator's own code.
    pub fn create(self, files: &[(&str, String)]) -> miette::Result<()> {
        for (relative, contents) in files {
            self.write(relative, contents)?;
        }
        Ok(())
    }

    fn write(&self, relative: &str, contents: &str) -> miette::Result<()> {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| self.write_failed(parent, error))?;
        }
        std::fs::write(&path, contents).map_err(|error| self.write_failed(&path, error))
    }

    fn write_failed(&self, path: &Path, error: std::io::Error) -> miette::Report {
        failure::error(self.write_code, format!("{}: {error}", path.display()))
    }
}

/// A name usable as a Cargo package and a directory.
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
    let project = Generator::new(name, parent, "zup.preset.init")
        .map_err(|error| failure::error("zup.preset.init_name", error))?;
    let name = project.name().to_owned();
    project.create(&[
        ("Cargo.toml", manifest(&name)),
        ("src/main.rs", MAIN.to_owned()),
        ("zup.preset.dev.toml", DEVELOPMENT.to_owned()),
        (".gitignore", GITIGNORE.to_owned()),
    ])
}

fn manifest(name: &str) -> String {
    format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"
description = "A zup installer preset"
publish = false

# The whole of what a preset needs. The GPUI stack this builds against, and the
# crates its settings are built from, are the ones this SDK was built with.
[dependencies]
zup-sdk = {{ version = "0.1.0", features = ["preset"] }}

# GPUI is a very large dependency tree, and compiling it at `opt-level = 0` is
# both slow and, for a text and layout engine, surprisingly slow at run time.
# These are the packages that dominate the cost; everything else stays at the
# development default so a change to this preset's own code is compiled in
# seconds.
#
# A name here that the GPUI stack has since renamed is a warning rather than an
# error, so it costs nothing but the optimisation it was buying. Cargo prints it
# on the first build, which is where it is worth knowing about.
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
[profile.dev.package.smol]
opt-level = 2
"#
    )
}

const MAIN: &str = r#"use zup_sdk::preset::prelude::*;

/// What an application may configure about how this preset looks.
///
/// `#[settings]` applies the three things Zup needs: a default, the
/// deserializer the host's configuration arrives through, and the JSON Schema
/// that validates an application's settings. That is why this project declares
/// the SDK and nothing else.
#[zup_sdk::preset::settings]
pub struct Settings {
    /// A line above the product name.
    pub hero: Option<String>,
    /// A file this application provides, named relative to its project.
    pub logo: Option<AssetRef>,
    /// A `RRGGBB` accent colour.
    pub accent: Option<String>,
}

/// The preset.
///
/// Named rather than `Preset` because a struct and a trait share one namespace,
/// and `impl Preset for Preset` would resolve the trait position to the struct.
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
    session: Session,
    state: Entity<SessionState>,
    settings: PresetSettings<Settings>,
    _state: Subscription,
    _settings: Subscription,
}

impl Render for View {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            let action = Action::SetComponent {
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
                .on_click(move |_, _, _| session.send(Action::Install)),
        )
    }
}

fn main() {
    if let Err(error) = zup_sdk::preset::run::<Aurora>() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
"#;

const DEVELOPMENT: &str = r##"# The application `zup preset dev` presents.
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

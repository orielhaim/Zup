//! The preset's own type, in a library so a test can name it without building
//! the binary.

use zup_sdk::preset::prelude::{gpui, *};

/// What an application may configure about how this preset looks.
///
/// `#[settings]` rather than naming the derives, so this project depends on the
/// SDK alone. The schema `zup preset pack` puts in the package is generated
/// from these same fields, by the same `schemars` this preset deserializes
/// with.
#[zup_sdk::preset::settings]
#[derive(Debug, Clone)]
pub struct Settings {
    /// A line above the product name.
    pub hero: Option<String>,
    /// A file this application provides, named relative to its project.
    pub logo: Option<AssetRef>,
    /// A `RRGGBB` accent colour.
    pub accent: Option<String>,
}

/// The Aurora preset.
pub struct Aurora;

impl Preset for Aurora {
    const NAME: &'static str = env!("CARGO_PKG_NAME");
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    type Settings = Settings;

    fn required_capabilities() -> Capabilities {
        Capabilities::new([Capability::Components, Capability::PlanPreview])
    }

    fn launch(context: PresetContext<Self::Settings>, cx: &mut App) {
        let session = context.session().clone();
        let settings = context.settings().clone();
        let state = session.state();
        let _changes = cx.observe(&state, |_, _| {});
        gpui::open_window(
            gpui::WindowOptions::default(),
            cx,
            move |window, cx| {
                let location = cx.new(|cx| {
                    gpui::component::input::InputState::new(window, cx)
                        .default_value(settings.read(cx).hero.clone().unwrap_or_default())
                });
                let view = cx.new(|_| View {
                    session,
                    state,
                    settings: settings.clone(),
                    location,
                    _settings: gpui::Subscription::new(|| {}),
                });
                let refreshed = view.clone();
                view.update(cx, |view, cx| {
                    view._settings = settings.observe(cx, move |_, cx| {
                        refreshed.update(cx, |_, cx| cx.notify());
                    });
                });
                view
            },
        )
        .expect("open the installer window");
    }
}

/// A window that draws what the host published and asks for what it wants.
pub struct View {
    session: zup_sdk::preset::Session,
    state: gpui::Entity<zup_sdk::preset::SessionState>,
    settings: zup_sdk::preset::PresetSettings<Settings>,
    location: gpui::Entity<gpui::component::input::InputState>,
    _settings: gpui::Subscription,
}

impl gpui::Render for View {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        use gpui::base::{Disableable, StyledExt};
        use gpui::component::button::Button;
        use gpui::component::{ActiveTheme, Theme};
        use gpui::{FontWeight, ParentElement, Styled, div, px};

        let theme: &Theme = cx.theme();
        let Some(snapshot) = self.state.read(cx).snapshot() else {
            return div()
                .p_6()
                .text_color(theme.colors.muted_foreground)
                .child("Waiting for the installer…");
        };

        let mut body = div()
            .v_flex()
            .gap_4()
            .p_6()
            .w_full()
            .max_w(px(720.0))
            .bg(theme.colors.background)
            .text_color(theme.colors.foreground)
            .child(
                div()
                    .text_size(px(21.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(snapshot.product.name.clone()),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.colors.muted_foreground)
                    .child(snapshot.product.version.clone()),
            );

        body = body.child(div().text_size(px(13.0)).child(
            self.settings.read(cx).hero.clone().unwrap_or_default(),
        ));
        body = body.child(div().text_size(px(13.0)).child(format!(
            "accent {}",
            self.settings.read(cx).accent.clone().unwrap_or_default()
        )));

        for component in snapshot.surface.components() {
            let action = Action::SetComponent {
                component: component.id.clone(),
                selected: !component.selected,
            };
            let session = self.session.clone();
            body = body.child(
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
        body = body.child(
            Button::new("install")
                .label("Install")
                .on_click(move |_, _, _| session.send(Action::Install)),
        );
        let _ = &self.location;
        body
    }
}

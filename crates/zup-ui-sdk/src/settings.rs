//! What the application configured, as something a view observes.
//!
//! Settings are part of the session rather than a value handed over once. The
//! host owns them, it may replace them while a session is running, and a preset
//! that read them into a field at launch would be drawing yesterday's
//! configuration with no way to hear about the current one.
//!
//! So they are an entity. [`PresetSettings`] derefs to it, so `cx.observe` and
//! `Entity::read` work on it directly, and the two methods here are conveniences
//! for the common case. There is no way to write them, because the host is the
//! only thing that may decide what an application configured.

use std::ops::Deref;

use gpui_kit::{App, AsyncApp, Entity, Subscription};

/// The settings the host sent, as something a view observes.
///
/// ```no_run
/// # use gpui_kit::{Context, IntoElement, ParentElement, Render, Window, div};
/// # use zup_ui_sdk::prelude::*;
/// # struct Settings { hero: Option<String> }
/// # struct View { settings: PresetSettings<Settings> }
/// impl Render for View {
///     fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
///         let hero = self.settings.read(cx).hero.clone();
///         div().child(hero.unwrap_or_default())
///     }
/// }
/// ```
pub struct PresetSettings<TSettings> {
    entity: Entity<TSettings>,
}

impl<TSettings: 'static> PresetSettings<TSettings> {
    pub(crate) fn new(entity: Entity<TSettings>) -> Self {
        Self { entity }
    }

    /// The settings currently in force.
    pub fn read<'a>(&self, cx: &'a App) -> &'a TSettings {
        self.entity.read(cx)
    }

    /// Run `on_notify` whenever the host replaces these settings.
    pub fn observe(
        &self,
        cx: &mut App,
        on_notify: impl FnMut(Entity<TSettings>, &mut App) + 'static,
    ) -> Subscription {
        cx.observe(&self.entity, on_notify)
    }

    /// Replace the settings, and tell everything observing them.
    pub(crate) fn replace(&self, cx: &mut AsyncApp, settings: TSettings) {
        let entity = self.entity.clone();
        cx.update(|cx| {
            entity.update(cx, |current, cx| {
                *current = settings;
                cx.notify();
            });
        });
    }
}

impl<TSettings: 'static> Deref for PresetSettings<TSettings> {
    type Target = Entity<TSettings>;

    fn deref(&self) -> &Self::Target {
        &self.entity
    }
}

impl<TSettings: 'static> Clone for PresetSettings<TSettings> {
    fn clone(&self) -> Self {
        Self {
            entity: self.entity.clone(),
        }
    }
}

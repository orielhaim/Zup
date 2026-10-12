use std::ops::Deref;

use gpui_kit::{App, AsyncApp, Entity, Subscription};

pub struct PresetSettings<TSettings> {
    entity: Entity<TSettings>,
}

impl<TSettings: 'static> PresetSettings<TSettings> {
    pub(crate) fn new(entity: Entity<TSettings>) -> Self {
        Self { entity }
    }

    pub fn read<'a>(&self, cx: &'a App) -> &'a TSettings {
        self.entity.read(cx)
    }

    pub fn observe(
        &self,
        cx: &mut App,
        on_notify: impl FnMut(Entity<TSettings>, &mut App) + 'static,
    ) -> Subscription {
        cx.observe(&self.entity, on_notify)
    }

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

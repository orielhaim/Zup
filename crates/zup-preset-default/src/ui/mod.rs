mod diagnostic;
mod fields;
mod identity;
mod maintenance;
mod operation;
mod parts;
pub mod plan;

pub use diagnostic::{BlockedView, DiagnosticView};
pub use fields::{ComponentChoice, Disclosure, GroupSummary, PathChooser, ScopeChoice};
pub use identity::{AppIdentity, AppMark, MarkBadge};
pub use maintenance::{ActionRow, DestructiveSection, Emphasis, HealthNotice, UpdateRow};
pub use operation::{OperationProgress, OutcomeView};
pub use parts::{
    ActionBar, Callout, InstallSummary, Section, Tone, caption, muted, title, tone_color,
};

use std::rc::Rc;

use zup_sdk::preset::gpui_kit::{App, Window};

pub type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

pub type ChoiceHandler<T> = Rc<dyn Fn(T, &mut Window, &mut App)>;

pub fn handler(f: impl Fn(&mut Window, &mut App) + 'static) -> Handler {
    Rc::new(f)
}

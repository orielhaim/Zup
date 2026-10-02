//! The installer-domain components the screens are built from.
//!
//! Each one is a concept a person recognizes - the application's identity, a
//! location, a choice of who it is for, what an operation is doing - drawn with
//! GPUI Kit's primitives underneath. Screens arrange these and never reach for
//! raw layout to say something one of them already says.

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

use zup_ui_sdk::gpui_kit::{App, Window};

/// What a control does when it is used.
pub type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

/// Wrap a closure as a [`Handler`].
pub fn handler(f: impl Fn(&mut Window, &mut App) + 'static) -> Handler {
    Rc::new(f)
}

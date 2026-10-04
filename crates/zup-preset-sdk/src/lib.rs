#![deny(unsafe_code)]

//! The implementation layer beneath `zup_sdk::preset`.
//!
//! A preset is an ordinary GPUI application that happens to talk to a zup
//! installer. It draws what the host publishes and asks for what a person
//! clicked, and everything zup hands it is a session, its settings, and its
//! assets. This crate is that arrangement: the [`Preset`] trait, the [`Session`]
//! it draws from, and the handshake that connects the two.
//!
//! Authors depend on `zup-sdk`, which is this crate's published surface; see
//! `zup_sdk::preset` for what that surface is. What lives here is the wire, the
//! settings machinery and the asset plumbing that make the trait above
//! possible.
//!
//! A preset reaches the host through [`Session::send`] and sees the host's state
//! through [`SessionState`]. It has no access to a plan, a transaction, an
//! elevation decision, or a package: that boundary is what makes a third-party
//! preset safe to ship, and the host validates every action against the state it
//! owns, so a preset asking for something this installation cannot do is refused
//! rather than obeyed.

mod asset;
mod preset;
mod session;
mod settings;
mod transport;

pub mod prelude;
pub mod presentation;

pub use asset::{ApplicationAssets, AssetRef, PresetAssets};
pub use preset::{
    Describe, NoSettings, Preset, PresetContext, PresetError, Settings, describe, run, serve,
};
pub use session::{ActionSender, Session, SessionState};
pub use settings::PresetSettings;

/// GPUI's headless test harness, for a preset that tests its own windows.
#[cfg(feature = "test-support")]
pub use gpui_kit::test as test_support;

pub use zup_preset_sdk_macros::settings;

// The transport is not re-exported. `Bootstrap` and `TransportError` appear in
// `serve`'s signature because a preset that has already been handed a bootstrap
// - which nothing a preset author has, since a preset is launched by a host -
// needs a way to hand it on. Everything else in `transport` is the handshake's
// own machinery, and a preset reaches the host through `Session::send`.

/// The crates a preset's settings are built from, resolved through `zup_sdk` by
/// the [`settings`] attribute.
///
/// Hidden because it is not an authoring surface: a preset applies the attribute
/// and never names these. Exposed under one path from here and from the facade
/// above, so one spelling reaches one set of versions whichever of the two a
/// preset depends on.
#[doc(hidden)]
pub mod __private {
    pub use schemars;
    pub use serde;
    pub use serde_json;
}

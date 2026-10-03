//! The `#[settings]` attribute.
//!
//! This crate is a build-time implementation detail of the preset SDK; nothing
//! here is authored against directly.

use proc_macro::TokenStream;
use quote::quote;

/// A preset's settings type.
///
/// Applies the three derives Zup needs from a settings type and nothing else,
/// pointing them at the versions the SDK was built against:
///
/// - `Deserialize`, so the host's configuration document arrives as this type
///   rather than as JSON the preset inspects by hand;
/// - `JsonSchema`, so the schema that validates an application's settings is
///   generated from the same definition the preset deserializes into, and the
///   two cannot disagree;
/// - `Default`, so an application that configures nothing still produces a
///   usable settings value.
///
/// This is an attribute rather than a derive because a derive is appended to
/// what it annotates and cannot add another derive to it. It is also what lets a
/// preset project depend on the SDK alone: `serde` and `schemars` are named
/// through the SDK rather than declared by the author, so the schema that
/// validates an application's settings is always the schema this SDK generates.
///
/// The derives are reached through `zup_sdk`, which the SDKs that re-export
/// this macro both make that name resolve to themselves. `serde` and `schemars`
/// are attribute macros that take their crate as a string rather than a path,
/// so the same name is what makes them find the right copies.
#[proc_macro_attribute]
pub fn settings(_attribute: TokenStream, item: TokenStream) -> TokenStream {
    let item: proc_macro2::TokenStream = item.into();
    quote! {
        #[derive(Default)]
        #[derive(zup_sdk::__private::serde::Deserialize)]
        #[derive(zup_sdk::__private::schemars::JsonSchema)]
        #[serde(crate = "zup_sdk::__private::serde")]
        #[schemars(crate = "zup_sdk::__private::schemars")]
        #item
    }
    .into()
}

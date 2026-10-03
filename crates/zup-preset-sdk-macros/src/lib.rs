//! The `#[Settings]` attribute.
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
#[proc_macro_attribute]
pub fn settings(_attribute: TokenStream, item: TokenStream) -> TokenStream {
    let item: proc_macro2::TokenStream = item.into();
    // Named through `$crate`'s own parent, which is this crate: the SDK that
    // re-exports the macro is the one whose serde and schemars are being used,
    // whichever SDK that happens to be.
    let serde = quote!(::zup_sdk::__private::serde);
    let schemars = quote!(::zup_sdk::__private::schemars);
    quote! {
        #[derive(Default)]
        #[derive(#serde::Deserialize)]
        #[derive(#schemars::JsonSchema)]
        #[serde(crate = "zup_sdk::__private::serde")]
        #[schemars(crate = "zup_sdk::__private::schemars")]
        #item
    }
    .into()
}
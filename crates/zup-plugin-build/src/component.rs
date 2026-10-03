//! Turning a plugin's core module into the component a host loads.
//!
//! A Rust crate compiled for `wasm32-unknown-unknown` produces a core module,
//! and a core module is not what a host loads: it has no types, so it cannot say
//! what its exports take or return. The Component Model resolves that by
//! componentising the module against a world - here, the plugin world the ABI
//! crate owns.
//!
//! This is Zup's step rather than the author's for two reasons. It needs a
//! componentiser whose version has to agree with the runtime that will load the
//! result, which is a version Zup controls; and it needs the world, which is the
//! contract rather than something a plugin project chooses. Doing it here means
//! an author runs one command and never installs a tool to match a version.

use wit_component::ComponentEncoder;

/// Why a module could not be turned into a plugin component.
///
/// The componentiser's own message is carried through rather than replaced, so
/// the specific reason a module was refused survives into what an author reads.
#[derive(Debug, thiserror::Error)]
pub enum ComponentError {
    #[error("this is already a component rather than a core module")]
    AlreadyAComponent,
    #[error(
        "this does not build a WebAssembly module for a plugin; compile it with \
         `--target wasm32-unknown-unknown` and a `cdylib` crate type"
    )]
    NotAModule,
    #[error("the module does not implement the plugin world: {0}")]
    Incompatible(#[from] anyhow::Error),
}

/// Turn a core module into a component that implements the plugin world.
///
/// The module the SDK produced already carries its own type metadata - that is
/// what the bindings generator embeds, and it is what makes the world known
/// without this function being told it. So the module is componentised as it
/// is, and the WIT is not parsed here at all: the guest was generated from the
/// same contract the host validates against, and parsing it a second time would
/// only create a way for the two to disagree.
pub fn componentize(module: &[u8]) -> Result<Vec<u8>, ComponentError> {
    // A component opens with its own layer rather than a core module's
    // magic-and-version. Checking both means a caller that hands us something
    // already componentised, or something that is not Wasm at all, is told
    // which rather than met with a parse error about a shape it did not ask
    // about.
    if module.starts_with(b"\0asm\x0d\0\x01\0") {
        return Err(ComponentError::AlreadyAComponent);
    }
    if !module.starts_with(b"\0asm\x01\0\0\0") {
        return Err(ComponentError::NotAModule);
    }

    Ok(ComponentEncoder::default()
        .module(module)?
        // Validated rather than trusted: this is where a component is proved to
        // implement the world the host will link against, and finding out here
        // is far better than finding out at load on a user's machine.
        .validate(true)
        .encode()?)
}
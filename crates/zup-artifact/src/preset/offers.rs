//! What an installer can offer the preset it will present.
//!
//! A `.zupui` states what it cannot work without, and somebody has to answer
//! before one is composed and again before one is launched. Both answers come
//! from the same derivation over the same installer, so it lives here rather
//! than beside either caller: the build plane and the runtime both reach this
//! crate, and a rule that existed in two of them would be a rule that would
//! eventually be applied two different ways.
//!
//! Derived rather than declared, so a preset that needs something this
//! application cannot do is refused instead of meeting a control that does
//! nothing.

use zup_core::Installer;
use zup_preset_protocol::{Capabilities, Capability};

/// What this application can offer a preset, across every surface it has.
///
/// The broad answer, and the only one a build can give: a build composes an
/// installer, not a session, and a session's own surface is not known until
/// somebody launches it. A host narrows this for the session it is actually
/// doing, which is the only difference between the two answers.
pub fn offers(installer: &Installer) -> Capabilities {
    let mut capabilities = Capabilities::new([
        // A diagnostics summary and a plan preview are the installer's own
        // subject matter, and a preset that offers them costs nothing.
        Capability::Diagnostics,
        Capability::PlanPreview,
        Capability::InstallDirectory,
    ]);
    if !installer.components.is_empty() {
        capabilities = capabilities.with(Capability::Components);
    }
    if installer.updates.is_some() {
        capabilities = capabilities.with(Capability::Updates);
    }
    if !installer.launchers.is_empty() {
        capabilities = capabilities.with(Capability::Launch);
    }
    // A maintenance surface always exists to be reached; what a given launch may
    // offer of it is the host's decision, not the application's.
    capabilities.with(Capability::Maintenance)
}

/// What this installer provides, for one session.
///
/// The same answer [`offers`] gives, minus the surfaces this particular launch
/// is not. A fresh install offers no maintenance surface, and a preset that
/// needs one is refused before it is launched rather than opening a window whose
/// buttons would all be dead.
pub fn offers_for(installer: &Installer, maintenance: bool) -> Capabilities {
    if maintenance {
        offers(installer)
    } else {
        offers(installer).without(Capability::Maintenance)
    }
}

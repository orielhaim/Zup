//! Choosing a presentation, without conditional compilation at the call site.
//!
//! A build of the runtime has one frontend. Which one is a compile-time fact, so
//! the callers of a surface should not each carry a `cfg` to find out - they ask
//! for a surface and get either the real one or a refusal that names what is
//! missing.

use std::path::Path;

use zup_core::Frontend;
use zup_exec::LifecycleAction;
use zup_windows::EmbeddedBundle;

use crate::cli::LifecycleArgs;
use crate::context::RuntimeContext;
use crate::lifecycle::{PreparedRuntime, Verb};

/// The graphical surface, for a launch that named no operation.
pub fn graphical(
    context: RuntimeContext,
    executable: &Path,
    bundle: &EmbeddedBundle,
) -> miette::Result<()> {
    if context.frontend != Frontend::Gui {
        return Err(no_surface("graphical"));
    }
    #[cfg(feature = "gui")]
    {
        let _ = bundle;
        crate::host::surface(executable)
    }
    #[cfg(not(feature = "gui"))]
    {
        let _ = (executable, bundle);
        Err(no_surface("graphical"))
    }
}

/// The uninstall confirmation window, for Apps & Features on a GUI package.
pub fn graphical_uninstall(
    context: RuntimeContext,
    executable: &Path,
    bundle: &EmbeddedBundle,
    args: &crate::uninstall::UninstallArgs,
) -> miette::Result<()> {
    if context.frontend != Frontend::Gui {
        return Err(no_surface("graphical"));
    }
    #[cfg(feature = "gui")]
    {
        let _ = bundle;
        crate::host::uninstall_confirmation(executable, args)
    }
    #[cfg(not(feature = "gui"))]
    {
        let _ = (executable, bundle, args);
        Err(no_surface("graphical"))
    }
}

/// The graphical surface, for an invocation that named an operation.
pub fn graphical_for(context: RuntimeContext, args: LifecycleArgs) -> miette::Result<()> {
    #[cfg(feature = "gui")]
    {
        crate::host::surface_from_arguments(context, args)
    }
    #[cfg(not(feature = "gui"))]
    {
        let _ = (context, args);
        Err(no_surface("graphical"))
    }
}

/// The interactive console, for an invocation that asked to apply this package.
pub fn console_apply(context: RuntimeContext, args: LifecycleArgs) -> miette::Result<()> {
    #[cfg(feature = "console")]
    {
        crate::console::run_apply(context, args)
    }
    #[cfg(not(feature = "console"))]
    {
        let _ = (context, args);
        Err(no_surface("interactive console"))
    }
}

/// The interactive console, for an invocation that named a lifecycle.
pub fn console_verb(
    context: RuntimeContext,
    verb: Verb,
    args: LifecycleArgs,
) -> miette::Result<()> {
    #[cfg(feature = "console")]
    {
        crate::console::run(context, verb, args)
    }
    #[cfg(not(feature = "console"))]
    {
        let _ = (context, verb, args);
        Err(no_surface("interactive console"))
    }
}

/// The interactive console, launched with no operation named.
pub fn console_direct(executable: &Path, bundle: &EmbeddedBundle) -> miette::Result<()> {
    #[cfg(feature = "console")]
    {
        crate::console::direct_launch(executable, bundle)
    }
    #[cfg(not(feature = "console"))]
    {
        let _ = (executable, bundle);
        Err(no_surface("interactive console"))
    }
}

/// Confirm an uninstall, where this build can ask.
pub fn confirm_uninstall() -> miette::Result<bool> {
    #[cfg(feature = "console")]
    {
        crate::console::confirm_uninstall()
    }
    #[cfg(not(feature = "console"))]
    {
        Err(no_surface("interactive console"))
    }
}

/// Confirm an update, where this build can ask.
pub fn confirm_update() -> miette::Result<bool> {
    #[cfg(feature = "console")]
    {
        crate::console::confirm_update()
    }
    #[cfg(not(feature = "console"))]
    {
        Err(no_surface("interactive console"))
    }
}

/// A cancelled question.
pub fn cancelled() -> miette::Report {
    #[cfg(feature = "console")]
    {
        crate::console::cancelled()
    }
    #[cfg(not(feature = "console"))]
    {
        miette::miette!("cancelled")
    }
}

/// Run a prepared lifecycle with the console's progress bar and retry.
pub fn execute_console(prepared: PreparedRuntime, action: LifecycleAction) -> miette::Result<()> {
    #[cfg(feature = "console")]
    {
        crate::console::execute(prepared, action)
    }
    #[cfg(not(feature = "console"))]
    {
        let _ = (prepared, action);
        Err(no_surface("interactive console"))
    }
}

fn no_surface(kind: &str) -> miette::Report {
    miette::miette!(
        "this build of the setup runtime has no {kind} surface; use --non-interactive or --yes \
         with a machine-readable output format"
    )
}

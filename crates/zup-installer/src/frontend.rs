use std::path::Path;

use zup_core::Frontend;
use zup_exec::LifecycleAction;
use zup_windows::EmbeddedBundle;

use crate::cli::LifecycleArgs;
use crate::lifecycle::{PreparedRuntime, Verb};
use crate::run::RuntimeContext;

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

pub fn graphical_uninstall(
    context: RuntimeContext,
    executable: &Path,
    bundle: &EmbeddedBundle,
    args: &crate::maintenance::UninstallArgs,
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

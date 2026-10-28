use std::path::Path;

use zup_core::Frontend;
use zup_windows::EmbeddedBundle;

use crate::cli::LifecycleArgs;
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

pub fn confirm_uninstall() -> miette::Result<bool> {
    #[cfg(feature = "console")]
    {
        crate::console::Console::new(false).confirm("Uninstall this application?", false)
    }
    #[cfg(not(feature = "console"))]
    {
        Err(no_surface("interactive console"))
    }
}

pub fn confirm_update() -> miette::Result<bool> {
    #[cfg(feature = "console")]
    {
        crate::console::Console::new(false).confirm("Install this update?", false)
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

fn no_surface(kind: &str) -> miette::Report {
    miette::miette!(
        "this build of the setup runtime has no {kind} surface; use --non-interactive or --yes \
         with a machine-readable output format"
    )
}

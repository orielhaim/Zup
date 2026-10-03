//! A plugin that records what an installation selected.
//!
//! Plugins return declarations; Zup installs them. Nothing here runs a command
//! or writes a registry key, and the world this plugin implements imports
//! nothing at all - which is what makes a plugin safe to ship inside somebody
//! else's application.

use zup_sdk::plugin::prelude::*;

struct Configure;

impl Plugin for Configure {
    fn plan(context: Context) -> Result<Plan, Error> {
        let mut components = context.selected_components.clone();
        components.sort();

        let report = format!(
            "application: {}\nversion: {}\nplugin: {}\ninstall directory: {}\n\
             scope: {}\nselected components: {}\n",
            context.app_name,
            context.app_version,
            context.plugin_id,
            context.install_directory,
            context.scope.as_str(),
            if components.is_empty() {
                "none".to_owned()
            } else {
                components.join(", ")
            },
        );

        Ok(Plan::new()
            .generated_file(GeneratedFile::text(
                "${install}/configure.txt",
                report,
            ))
            .launcher(Launcher::menu(
                format!("{} Settings", context.app_name),
                "${launcher}",
            )))
    }
}

zup_sdk::plugin::export!(Configure);
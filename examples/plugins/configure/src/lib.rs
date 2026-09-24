wit_bindgen::generate!({
    path: "../../../wit",
    world: "plugin",
});

use exports::zup::plugin::planner::{
    Context, GeneratedFile, Guest, InstallationPlan, PluginError, ResourceItem,
};

struct ConfigurePlugin;

impl Guest for ConfigurePlugin {
    fn plan(context: Context) -> Result<InstallationPlan, PluginError> {
        let mut selected_components = context.selected_components;
        selected_components.sort();

        let contents = format!(
            "app id: {}\ninstall directory: {}\nselected components: {}\n",
            context.app_id,
            context.install_directory,
            selected_components.join(", "),
        )
        .into_bytes();

        Ok(InstallationPlan {
            resources: vec![ResourceItem::GeneratedFile(GeneratedFile {
                destination: "${install}/plugin-config.txt".to_owned(),
                contents,
            })],
        })
    }
}

export!(ConfigurePlugin);

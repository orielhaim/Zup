use schemars::JsonSchema;
use serde::Deserialize;
use zup_core::ProjectPath;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconConfig {
    pub source: ProjectPath,
    pub padding_milli: u16,
}

impl IconConfig {
    pub fn padding(&self) -> f32 {
        f32::from(self.padding_milli) / 1000.0
    }
}

pub(crate) fn from_value(value: toml::Value) -> Result<IconConfig, String> {
    match value {
        toml::Value::String(path) => Ok(IconConfig {
            source: project_path(&path)?,
            padding_milli: 0,
        }),
        toml::Value::Table(mut table) => {
            let source = table
                .remove("source")
                .ok_or_else(|| "icon table needs a `source` path".to_owned())?;
            let toml::Value::String(path) = source else {
                return Err("icon `source` must be a path".to_owned());
            };
            let padding_milli = match table.remove("padding") {
                None => 0,
                Some(toml::Value::Float(value)) => padding(value)?,
                Some(toml::Value::Integer(value)) => padding(value as f64)?,
                Some(_) => return Err("icon `padding` must be a number".to_owned()),
            };
            if let Some(unknown) = table.keys().next() {
                return Err(format!(
                    "icon has unknown field `{unknown}`. The supported fields are `source` and `padding`"
                ));
            }
            Ok(IconConfig {
                source: project_path(&path)?,
                padding_milli,
            })
        }
        _ => Err("icon must be a path, or a table with `source` and optional `padding`".to_owned()),
    }
}

fn project_path(path: &str) -> Result<ProjectPath, String> {
    ProjectPath::new(path).map_err(|error| format!("icon source: {error}"))
}

fn padding(value: f64) -> Result<u16, String> {
    if !value.is_finite() || value < 0.0 || value >= 0.5 {
        return Err(
            "icon padding must be at least 0 and less than 0.5. `0.10` leaves a tenth of the canvas empty on each side"
                .to_owned(),
        );
    }
    let milli = (value * 1000.0).round();
    if !(0.0..500.0).contains(&milli) {
        return Err(
            "icon padding must be at least 0 and less than 0.5. `0.10` leaves a tenth of the canvas empty on each side"
                .to_owned(),
        );
    }
    Ok(milli as u16)
}

#[derive(Debug, Deserialize, JsonSchema)]
#[allow(dead_code)]
#[serde(untagged, deny_unknown_fields)]
pub enum IconSetting {
    Source(ProjectPath),
    Options(IconOptions),
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub struct IconOptions {
    pub source: ProjectPath,
    #[serde(default)]
    pub padding: Option<f64>,
}

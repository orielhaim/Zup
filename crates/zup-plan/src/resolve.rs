use zup_core::{App, Template, Variable, VariableValue};

use crate::error::PlanError;

pub fn resolve_template(
    template: &Template,
    app: &App,
    install_directory: &Template,
    generation: Option<&zup_core::GenerationDescriptor>,
) -> Result<Template, PlanError> {
    let resolved = template.substitute(|variable| match variable {
        Variable::AppId => Some(VariableValue::Literal(app.id.to_string())),
        Variable::AppName => Some(VariableValue::Literal(app.name.to_string())),
        Variable::AppVersion => Some(VariableValue::Literal(app.version.to_string())),
        Variable::Install => Some(VariableValue::Template(install_directory.clone())),
        Variable::Payload => None,
        Variable::Location(_) => None,
    });

    debug_assert!(!resolved.contains_variable(Variable::Install));
    debug_assert!(!resolved.contains_variable(Variable::AppId));
    debug_assert!(!resolved.contains_variable(Variable::AppName));
    debug_assert!(!resolved.contains_variable(Variable::AppVersion));
    if resolved.contains_variable(Variable::Payload) && generation.is_none() {
        return Err(PlanError::TemplateResolutionError {
            variable: Variable::Payload,
        });
    }
    Ok(resolved)
}

pub fn resolve_install_directory(directory: &Template, app: &App) -> Result<Template, PlanError> {
    let resolved = directory.substitute(|variable| match variable {
        Variable::AppId => Some(VariableValue::Literal(app.id.to_string())),
        Variable::AppName => Some(VariableValue::Literal(app.name.to_string())),
        Variable::AppVersion => Some(VariableValue::Literal(app.version.to_string())),
        Variable::Install => Some(VariableValue::Literal(String::new())),
        Variable::Payload => None,
        Variable::Location(_) => None,
    });

    if directory.contains_variable(Variable::Install)
        || directory.contains_variable(Variable::Payload)
    {
        return Err(PlanError::RecursiveInstallDirectory);
    }
    Ok(resolved)
}

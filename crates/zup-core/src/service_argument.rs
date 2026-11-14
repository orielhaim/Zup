use crate::{InstallationId, ResourceId, ServiceId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceArgument<'a> {
    Literal(&'a str),
    DataDirectory(ResourceId),
    WindowsJob(ServiceId),
    NativeName(ServiceId),
}

impl<'a> ServiceArgument<'a> {
    pub fn parse(argument: &'a str) -> Result<Self, InvalidServiceArgument> {
        if !argument.contains("${data.")
            && !argument.contains("${job.")
            && !argument.contains("${service.")
        {
            return Ok(Self::Literal(argument));
        }
        let reference = argument
            .strip_prefix("${")
            .and_then(|value| value.strip_suffix('}'))
            .filter(|value| {
                !value
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '{' | '}'))
            })
            .ok_or(InvalidServiceArgument)?;
        if let Some(id) = reference.strip_prefix("data.") {
            let parsed = ResourceId::new(id).map_err(|_| InvalidServiceArgument)?;
            if parsed.as_str() == id {
                return Ok(Self::DataDirectory(parsed));
            }
        }
        if let Some(id) = reference.strip_prefix("job.") {
            let parsed = ServiceId::new(id).map_err(|_| InvalidServiceArgument)?;
            if parsed.as_str() == id {
                return Ok(Self::WindowsJob(parsed));
            }
        }
        if let Some(id) = reference.strip_prefix("service.") {
            let parsed = ServiceId::new(id).map_err(|_| InvalidServiceArgument)?;
            if parsed.as_str() == id {
                return Ok(Self::NativeName(parsed));
            }
        }
        Err(InvalidServiceArgument)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "a service reference must be a whole argument, such as ${{data.database}}, ${{job.worker}}, or ${{service.worker}}"
)]
pub struct InvalidServiceArgument;

pub fn service_job_name(installation: InstallationId, service: &ServiceId) -> String {
    let mut identity = b"zup:windows-service-job:1\0".to_vec();
    identity.extend(serde_json::to_vec(&(installation, service)).expect("job identity serializes"));
    format!(
        "Global\\zup-service-{}",
        crate::hash_bytes(&identity).to_hex()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[rstest::rstest]
    #[case("--job=${job.worker}")]
    #[case("--name=${service.worker}")]
    #[case("${service.}")]
    #[case("${job.}")]
    #[case("--database=${data.database}")]
    #[case("${data.database}/child")]
    #[case("${data.}")]
    #[case("${data. database}")]
    fn data_paths_require_a_complete_argument(#[case] argument: &str) {
        assert!(ServiceArgument::parse(argument).is_err());
    }

    #[test]
    fn data_references_preserve_literals_and_identify_the_logical_directory() {
        assert_eq!(
            ServiceArgument::parse("$HOME/literal").unwrap(),
            ServiceArgument::Literal("$HOME/literal")
        );
        assert_eq!(
            ServiceArgument::parse("${data.database}").unwrap(),
            ServiceArgument::DataDirectory(ResourceId::new("database").unwrap())
        );
        assert_eq!(
            ServiceArgument::parse("${job.worker}").unwrap(),
            ServiceArgument::WindowsJob(ServiceId::new("worker").unwrap())
        );
        assert_eq!(
            ServiceArgument::parse("${service.worker}").unwrap(),
            ServiceArgument::NativeName(ServiceId::new("worker").unwrap())
        );
    }
}

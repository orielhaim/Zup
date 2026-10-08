use zup_plugin_abi as abi;

pub use super::exports::zup::plugin::planner::{
    FileAssociation, GeneratedFile, Launcher, LauncherLocation, Protocol, Service, ServiceStart,
};
use super::exports::zup::plugin::planner::{PathEntry, PluginError, ResourceItem};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    code: String,
    message: String,
}

impl Error {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new("unsupported", message)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid-context", message)
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

impl From<Error> for PluginError {
    fn from(error: Error) -> Self {
        Self {
            code: error.code,
            message: error.message,
        }
    }
}

impl Launcher {
    pub fn menu(name: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            location: LauncherLocation::Menu,
            name: name.into(),
            target: target.into(),
            arguments: Vec::new(),
            working_directory: None,
        }
    }

    pub fn desktop(name: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            location: LauncherLocation::Desktop,
            name: name.into(),
            target: target.into(),
            arguments: Vec::new(),
            working_directory: None,
        }
    }

    #[must_use]
    pub fn with_arguments<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.arguments = arguments.into_iter().map(Into::into).collect();
        self
    }

    #[must_use]
    pub fn with_working_directory(mut self, directory: impl Into<String>) -> Self {
        self.working_directory = Some(directory.into());
        self
    }
}

impl Service {
    pub fn new(id: impl Into<String>, name: impl Into<String>, binary: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            display_name: None,
            binary: binary.into(),
            arguments: Vec::new(),
            start: ServiceStart::Manual,
        }
    }

    #[must_use]
    pub fn with_display_name(mut self, display_name: impl Into<String>) -> Self {
        self.display_name = Some(display_name.into());
        self
    }

    #[must_use]
    pub fn with_arguments<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.arguments = arguments.into_iter().map(Into::into).collect();
        self
    }

    #[must_use]
    pub fn with_start(mut self, start: ServiceStart) -> Self {
        self.start = start;
        self
    }
}

impl Protocol {
    pub fn new(scheme: impl Into<String>, executable: impl Into<String>) -> Self {
        Self {
            scheme: scheme.into(),
            executable: executable.into(),
            args: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_arguments<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = arguments.into_iter().map(Into::into).collect();
        self
    }
}

impl FileAssociation {
    pub fn new(
        id: impl Into<String>,
        extension: impl Into<String>,
        executable: impl Into<String>,
    ) -> Self {
        Self {
            extension: extension.into(),
            id: id.into(),
            description: None,
            executable: executable.into(),
        }
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

impl GeneratedFile {
    pub fn new(destination: impl Into<String>, contents: impl Into<Vec<u8>>) -> Self {
        Self {
            destination: destination.into(),
            contents: contents.into(),
        }
    }

    pub fn text(destination: impl Into<String>, contents: impl AsRef<str>) -> Self {
        Self::new(destination, contents.as_ref().as_bytes().to_vec())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path(String);

impl Path {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<Path> for ResourceItem {
    fn from(path: Path) -> Self {
        Self::PathEntry(PathEntry { value: path.0 })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    resources: Vec<ResourceItem>,
}

impl Plan {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn generated_file(mut self, file: GeneratedFile) -> Self {
        self.resources.push(ResourceItem::GeneratedFile(file));
        self
    }

    pub fn launcher(mut self, launcher: Launcher) -> Self {
        self.resources.push(ResourceItem::Launcher(launcher));
        self
    }

    pub fn path_entry(mut self, path: impl Into<Path>) -> Self {
        self.resources.push(path.into().into());
        self
    }

    pub fn service(mut self, service: Service) -> Self {
        self.resources.push(ResourceItem::Service(service));
        self
    }

    pub fn protocol(mut self, protocol: Protocol) -> Self {
        self.resources.push(ResourceItem::Protocol(protocol));
        self
    }

    pub fn file_association(mut self, association: FileAssociation) -> Self {
        self.resources
            .push(ResourceItem::FileAssociation(association));
        self
    }

    pub(crate) fn into_resources(self) -> Vec<ResourceItem> {
        self.resources
    }

    pub fn validate(&self) -> Result<(), Error> {
        if self.resources.len() > abi::MAX_PLAN_RESOURCES {
            return Err(Error::invalid(format!(
                "a plan may declare at most {} resources; this one declares {}",
                abi::MAX_PLAN_RESOURCES,
                self.resources.len()
            )));
        }
        Ok(())
    }
}

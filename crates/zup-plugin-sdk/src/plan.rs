//! The plan a plugin returns: the declarative resources Zup will install.
//!
//! The resource shapes are the WIT's own, because they are what crosses the ABI
//! and what the host matches on. What this module adds is the construction:
//! each one has an associated constructor that supplies its defaults, so a
//! plugin names the field it means and not the shape of the whole record.

use zup_plugin_abi as abi;

use crate::planner::{
    LauncherLocation, PathEntry, PluginError, ResourceItem, ServiceStart,
};

// The resource records are the WIT's own, re-exported from `crate::planner`.
// They are re-exported here too so a plugin can name them without a second
// import, and so the constructors below are visibly methods on the same types
// rather than on look-alikes.
pub use crate::planner::{
    FileAssociation, GeneratedFile, Launcher, Protocol, Service,
};

/// Why a plugin cannot produce a plan.
///
/// A plugin refuses by returning this rather than by trapping, because a
/// refusal is an answer the host can show a person and act on while a trap is a
/// fault. The code is what a host branches on and what someone reading a log
/// matches; the message is what they read once it has matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    code: String,
    message: String,
}

impl Error {
    /// Refuse for a reason the plugin's author named.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// The plugin cannot do what was asked of it here.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new("unsupported", message)
    }

    /// The context the plugin was given does not permit a plan.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid-context", message)
    }

    /// The code a host branches on.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// What the person reading it is told.
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

/// A start-menu or desktop entry.
///
/// The WIT's record with a constructor, so a plugin does not have to name
/// [`LauncherLocation`] to say which one it means.
impl Launcher {
    /// An entry in the start menu.
    pub fn menu(name: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            location: LauncherLocation::Menu,
            name: name.into(),
            target: target.into(),
            arguments: Vec::new(),
            working_directory: None,
        }
    }

    /// An entry on the desktop.
    pub fn desktop(name: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            location: LauncherLocation::Desktop,
            name: name.into(),
            target: target.into(),
            arguments: Vec::new(),
            working_directory: None,
        }
    }

    /// Arguments passed to the target.
    #[must_use]
    pub fn with_arguments<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.arguments = arguments.into_iter().map(Into::into).collect();
        self
    }

    /// The directory the target starts in.
    #[must_use]
    pub fn with_working_directory(mut self, directory: impl Into<String>) -> Self {
        self.working_directory = Some(directory.into());
        self
    }
}

impl Service {
    /// A service with an identifier and the binary it runs.
    ///
    /// The identifier is what uninstall refers to and what a later install
    /// matches on, so it should survive a version bump. The name is the
    /// internal service name and is rarely what an author means to change.
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

    /// The name shown in the services list.
    #[must_use]
    pub fn with_display_name(mut self, display_name: impl Into<String>) -> Self {
        self.display_name = Some(display_name.into());
        self
    }

    /// Arguments passed to the service binary.
    #[must_use]
    pub fn with_arguments<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.arguments = arguments.into_iter().map(Into::into).collect();
        self
    }

    /// When the service starts.
    #[must_use]
    pub fn with_start(mut self, start: ServiceStart) -> Self {
        self.start = start;
        self
    }
}

impl Protocol {
    /// A URL scheme handled by an executable.
    pub fn new(scheme: impl Into<String>, executable: impl Into<String>) -> Self {
        Self {
            scheme: scheme.into(),
            executable: executable.into(),
            args: Vec::new(),
        }
    }

    /// Arguments passed with a launched URL.
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
    /// An association for one file extension.
    ///
    /// The identifier is derived from the extension because it has to be stable
    /// across versions and unique on the machine, and the extension is the only
    /// part of this an author actually chooses.
    pub fn for_extension(extension: impl Into<String>, executable: impl Into<String>) -> Self {
        let extension = extension.into();
        let id = format!("Zup.{}", extension.trim_start_matches('.').to_uppercase());
        Self {
            extension,
            id,
            description: None,
            executable: executable.into(),
        }
    }

    /// The name shown for the file type.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

impl GeneratedFile {
    /// A file written during install, from bytes.
    pub fn new(destination: impl Into<String>, contents: impl Into<Vec<u8>>) -> Self {
        Self {
            destination: destination.into(),
            contents: contents.into(),
        }
    }

    /// A file written during install, from text.
    pub fn text(destination: impl Into<String>, contents: impl AsRef<str>) -> Self {
        Self::new(destination, contents.as_ref().as_bytes().to_vec())
    }
}

/// A directory to add to the system `PATH`.
///
/// A newtype so a plan that adds a path entry is saying something specific: the
/// type is what says it, and it converts to the WIT variant directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path(String);

impl Path {
    /// One directory to add.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The directory.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<Path> for ResourceItem {
    fn from(path: Path) -> Self {
        Self::PathEntry(PathEntry { value: path.0 })
    }
}

/// The declaration a plugin returns.
///
/// A builder rather than a record with a `resources` field, because every plugin
/// writes the same shape: start empty, declare two or three things, return it.
/// Each method takes and returns the plan, so a plugin reads as one line per
/// resource.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    resources: Vec<ResourceItem>,
}

impl Plan {
    /// A plan that declares nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// A file written during install.
    pub fn generated_file(mut self, file: GeneratedFile) -> Self {
        self.resources.push(ResourceItem::GeneratedFile(file));
        self
    }

    /// A start-menu or desktop entry.
    pub fn launcher(mut self, launcher: Launcher) -> Self {
        self.resources.push(ResourceItem::Launcher(launcher));
        self
    }

    /// An entry to add to the system `PATH`.
    pub fn path_entry(mut self, path: impl Into<Path>) -> Self {
        self.resources.push(path.into().into());
        self
    }

    /// A Windows service.
    pub fn service(mut self, service: Service) -> Self {
        self.resources.push(ResourceItem::Service(service));
        self
    }

    /// A URL scheme to register.
    pub fn protocol(mut self, protocol: Protocol) -> Self {
        self.resources.push(ResourceItem::Protocol(protocol));
        self
    }

    /// A file type to associate with the application.
    pub fn file_association(mut self, association: FileAssociation) -> Self {
        self.resources
            .push(ResourceItem::FileAssociation(association));
        self
    }

    /// Add a resource the SDK does not have a constructor for.
    pub fn resource(mut self, resource: ResourceItem) -> Self {
        self.resources.push(resource);
        self
    }

    /// What this plan declares, in the order it was added.
    pub fn resources(&self) -> &[ResourceItem] {
        &self.resources
    }

    /// Take what this plan declares.
    pub(crate) fn into_resources(self) -> Vec<ResourceItem> {
        self.resources
    }

    /// Whether this plan declares nothing.
    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }

    /// Refuse a plan larger than the ABI allows, before it reaches the host.
    ///
    /// The host enforces the same limit, but a plugin told at the point of the
    /// mistake does not need a host running to find out about it.
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
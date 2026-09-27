//! The compatibility contract for a zup toolchain component.
//!
//! `zup build` composes an installer out of binaries zup itself produced: a
//! native runtime template, and a dispatcher for a universal artifact. Those bytes
//! have a version, a machine, and a presentation, and an installer composed from
//! the wrong combination is an installer that does not work on the machine it was
//! built for — sometimes on any machine.
//!
//! A file name is not a compatibility check. `zup-setup-gui.exe` is a name, and
//! the same name is written by every zup release that ever had a GUI template. So
//! every component ships a machine-readable descriptor beside it, and the
//! descriptor is what the builder reads.
//!
//! The contract has one more property, and it is the reason the descriptor exists
//! as a file rather than as a field inside the binary: **a build host must be able
//! to inspect a component it cannot run.** A Linux build host composes a Windows
//! installer for two architectures; neither binary can be started there. Reading
//! the descriptor, and the target and subsystem out of the file's own header,
//! answers "will this work" without executing anything.
//!
//! This crate is deliberately portable and dependency-light. It is read by the
//! developer CLI, which is Windows-only, and written by the repository's
//! contributor tooling, which is not.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zup_core::Frontend;

/// The version of this contract.
///
/// Bumped when the descriptor's shape or its meaning changes. A builder refuses a
/// component whose descriptor it does not understand, rather than reading the
/// fields it recognises and ignoring the rest.
pub const FORMAT_VERSION: u32 = 1;

/// The extension of the descriptor written beside every component.
pub const DESCRIPTOR_SUFFIX: &str = ".zup-toolchain.json";

/// What a component is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentKind {
    /// A native installer runtime template, composed into a target's installer.
    Runtime,
    /// A universal-artifact launcher, composed into a universal installer.
    Dispatcher,
}

/// What a component is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolchainComponent {
    /// The runtime template for one target and one frontend.
    Runtime {
        target: zup_core::TargetTriple,
        frontend: Frontend,
    },
    /// The launcher for one presentation experience.
    ///
    /// `online` is a capability rather than a variant: a thin artifact's launcher
    /// resolves a release over the network before it can start a runtime, and the
    /// same source built without that path refuses a thin artifact with a clear
    /// reason instead of pretending. An offline artifact needs neither.
    Dispatcher { subsystem: Subsystem, online: bool },
}

/// The presentation a component targets, as the artifact model names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Subsystem {
    Gui,
    Console,
}

impl Subsystem {
    /// The launcher experience a frontend's installer needs.
    pub fn for_frontend(frontend: Frontend) -> Self {
        match frontend {
            Frontend::Gui => Self::Gui,
            Frontend::Console | Frontend::Headless => Self::Console,
        }
    }

    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gui => "gui",
            Self::Console => "console",
        }
    }

    /// Whether this subsystem can launch a component presenting `frontend`.
    pub const fn carries(self, frontend: Frontend) -> bool {
        matches!(
            (self, frontend),
            (Self::Gui, Frontend::Gui) | (Self::Console, Frontend::Console | Frontend::Headless)
        )
    }
}

/// The file name a component is stored under, with `executable_suffix` appended.
///
/// The suffix is a parameter rather than a `cfg!` because this crate is portable
/// and a portable crate may not branch on the build host. It is also more honest:
/// the suffix is a property of the machine the file will be written on, which is
/// the caller's machine and not this contract's.
///
/// The machine is in the name because a host build writes the unsuffixed name for
/// its own machine: a build that found a host image here would compose a
/// universal artifact whose launcher is too wide to run on the machines the
/// artifact exists to serve.
pub fn file_name(component: &ToolchainComponent, executable_suffix: &str) -> String {
    match component {
        ToolchainComponent::Runtime { target, frontend } => {
            format!(
                "zup-setup-{}-{}{}",
                frontend.as_str(),
                target.as_str(),
                executable_suffix
            )
        }
        ToolchainComponent::Dispatcher { subsystem, online } => {
            let online = if *online { "-online" } else { "" };
            format!(
                "zup-dispatch-{}{online}-{}{executable_suffix}",
                subsystem.as_str(),
                DISPATCHER_TARGET
            )
        }
    }
}

/// The machine a dispatcher is built for.
///
/// It has to be the narrowest machine any variant can serve, which on Windows is
/// always 32-bit x86: it runs natively on x86, under WOW64 on x64, and under the
/// x86 compatibility layer on arm64. It is also the smallest of the three, which
/// matters for a file that is downloaded before any of it has been needed.
pub const DISPATCHER_TARGET: &str = "i686-pc-windows-msvc";

/// The descriptor written beside a component.
pub fn descriptor_file_name(component: &ToolchainComponent, executable_suffix: &str) -> String {
    format!(
        "{}{DESCRIPTOR_SUFFIX}",
        file_name(component, executable_suffix)
    )
}

/// The compatibility identity of one component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentDescriptor {
    /// The contract version this descriptor was written against.
    pub format_version: u32,
    /// Whether this is a runtime or a dispatcher.
    pub kind: ComponentKind,
    /// The zup release that produced it.
    pub zup_version: String,
    /// The canonical target triple the bytes are for.
    pub target: String,
    /// The presentation the component presents, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontend: Option<String>,
    /// The launcher experience, when the component is a dispatcher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subsystem: Option<String>,
    /// Whether a dispatcher can resolve a release over the network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub online: Option<bool>,
    /// The SHA-256 of the component's bytes, hex.
    pub digest: String,
    /// The component's size in bytes.
    pub size: u64,
}

impl ComponentDescriptor {
    /// A descriptor for a component whose bytes are at `path`.
    pub fn of(
        component: &ToolchainComponent,
        zup_version: &str,
        path: &Path,
    ) -> Result<Self, ToolchainError> {
        let (size, digest) = measure(path)?;
        Ok(match component {
            ToolchainComponent::Runtime { target, frontend } => Self {
                format_version: FORMAT_VERSION,
                kind: ComponentKind::Runtime,
                zup_version: zup_version.to_owned(),
                target: target.as_str().to_owned(),
                frontend: Some(frontend.as_str().to_owned()),
                subsystem: None,
                online: None,
                digest,
                size,
            },
            ToolchainComponent::Dispatcher { subsystem, online } => Self {
                format_version: FORMAT_VERSION,
                kind: ComponentKind::Dispatcher,
                zup_version: zup_version.to_owned(),
                target: "i686-pc-windows-msvc".to_owned(),
                frontend: None,
                subsystem: Some(subsystem.as_str().to_owned()),
                online: Some(*online),
                digest,
                size,
            },
        })
    }

    /// The document written beside the component.
    pub fn encode(&self) -> String {
        serde_json::to_string_pretty(self).expect("a descriptor is always serializable")
    }

    /// Read a descriptor.
    ///
    /// The error carries no path: the caller knows which file it read, and a
    /// second copy of the path in the error is a second place for it to be wrong.
    pub fn parse(bytes: &[u8]) -> Result<Self, ToolchainError> {
        serde_json::from_slice(bytes).map_err(|error| ToolchainError::Malformed(error.to_string()))
    }

    /// Whether this descriptor is the component `wanted`, for `zup_version`.
    ///
    /// The version check is the point of the whole exercise: a stale runtime from
    /// another zup release would produce an installer with a plan format the
    /// template cannot read, and the failure would surface on a user's machine
    /// rather than on the build host.
    pub fn check(
        &self,
        wanted: &ToolchainComponent,
        zup_version: &str,
    ) -> Result<(), ToolchainError> {
        if self.format_version != FORMAT_VERSION {
            return Err(ToolchainError::Incompatible(format!(
                "`{}` was written against toolchain format {} and this zup speaks format {}",
                self.zup_version, self.format_version, FORMAT_VERSION
            )));
        }
        if self.zup_version != zup_version {
            return Err(ToolchainError::Incompatible(format!(
                "component is from zup {} and this is zup {zup_version}",
                self.zup_version
            )));
        }
        match wanted {
            ToolchainComponent::Runtime { target, frontend } => {
                if self.kind != ComponentKind::Runtime {
                    return Err(ToolchainError::WrongKind);
                }
                if self.target != target.as_str() {
                    return Err(ToolchainError::WrongTarget {
                        found: self.target.clone(),
                        wanted: target.as_str().to_owned(),
                    });
                }
                if self.frontend.as_deref() != Some(frontend.as_str()) {
                    return Err(ToolchainError::WrongFrontend {
                        found: self.frontend.clone().unwrap_or_else(|| "none".to_owned()),
                        wanted: frontend.as_str().to_owned(),
                    });
                }
            }
            ToolchainComponent::Dispatcher { subsystem, online } => {
                if self.kind != ComponentKind::Dispatcher {
                    return Err(ToolchainError::WrongKind);
                }
                if self.subsystem.as_deref() != Some(subsystem.as_str()) {
                    return Err(ToolchainError::WrongSubsystem {
                        found: self.subsystem.clone().unwrap_or_else(|| "none".to_owned()),
                        wanted: subsystem.as_str().to_owned(),
                    });
                }
                // A launcher that cannot fetch a release refuses a thin artifact
                // with a clear reason rather than pretending, which is the right
                // behaviour for an offline build and the wrong one for a thin
                // release. The capability is part of the component's identity, so
                // asking for the wrong one is a refusal rather than a surprise on
                // a user's machine.
                if self.online.unwrap_or(false) != *online {
                    return Err(ToolchainError::WrongCapability {
                        found: if self.online.unwrap_or(false) {
                            "online"
                        } else {
                            "offline"
                        },
                        wanted: if *online { "online" } else { "offline" },
                    });
                }
            }
        }
        Ok(())
    }
}

/// Why a component could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ToolchainError {
    #[error("no toolchain component at `{path}`")]
    Missing { path: PathBuf },
    #[error("`{path}` is not a usable toolchain component: {reason}")]
    Unreadable { path: PathBuf, reason: String },
    #[error("a toolchain component descriptor is not valid: {0}")]
    Malformed(String),
    #[error("{0}")]
    Incompatible(String),
    #[error("component is a dispatcher and a runtime was wanted, or the other way round")]
    WrongKind,
    #[error("component is for `{found}` and `{wanted}` was wanted")]
    WrongTarget { found: String, wanted: String },
    #[error("component presents `{found}` and `{wanted}` was wanted")]
    WrongFrontend { found: String, wanted: String },
    #[error("component is a `{found}` launcher and a `{wanted}` one was wanted")]
    WrongSubsystem { found: String, wanted: String },
    #[error("component is the {found} launcher and the {wanted} one was wanted")]
    WrongCapability {
        found: &'static str,
        wanted: &'static str,
    },
    #[error("`{path}` is {found} bytes and the descriptor says {wanted}")]
    WrongSize {
        path: PathBuf,
        found: u64,
        wanted: u64,
    },
    #[error("`{path}` does not hash to the digest its descriptor names")]
    WrongDigest { path: PathBuf },
}

/// The descriptor that belongs to a component path.
///
/// Derived from the file that is actually there, not from a component identity,
/// because a caller holding a path is holding a file: a name composed from the
/// target and the frontend would disagree with a renamed file and produce a
/// "missing descriptor" for a component that has one.
fn descriptor_path_for(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}{DESCRIPTOR_SUFFIX}"))
}

/// The size and SHA-256 of a file.
pub fn measure(path: &Path) -> Result<(u64, String), ToolchainError> {
    let bytes = std::fs::read(path).map_err(|error| ToolchainError::Unreadable {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok((bytes.len() as u64, hex(&hasher.finalize())))
}

/// Confirm that a component's bytes are the ones its descriptor names.
///
/// The descriptor is a claim; this is the check. A component that was replaced,
/// truncated, or partially copied fails here rather than producing an installer
/// nobody can diagnose.
pub fn verify_bytes(path: &Path, descriptor: &ComponentDescriptor) -> Result<(), ToolchainError> {
    let (size, digest) = measure(path)?;
    if size != descriptor.size {
        return Err(ToolchainError::WrongSize {
            path: path.to_path_buf(),
            found: size,
            wanted: descriptor.size,
        });
    }
    if digest != descriptor.digest {
        return Err(ToolchainError::WrongDigest {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// Read and check a component and its descriptor together.
pub fn read(
    path: &Path,
    wanted: &ToolchainComponent,
    zup_version: &str,
) -> Result<ComponentDescriptor, ToolchainError> {
    let descriptor_path = descriptor_path_for(path);
    let bytes = std::fs::read(&descriptor_path).map_err(|error| ToolchainError::Unreadable {
        path: descriptor_path.clone(),
        reason: error.to_string(),
    })?;
    let descriptor =
        ComponentDescriptor::parse(&bytes).map_err(|error| ToolchainError::Unreadable {
            path: descriptor_path.clone(),
            reason: error.to_string(),
        })?;
    descriptor
        .check(wanted, zup_version)
        .map_err(|error| annotate(error, &descriptor_path))?;
    verify_bytes(path, &descriptor).map_err(|error| annotate(error, &descriptor_path))?;
    Ok(descriptor)
}

fn annotate(error: ToolchainError, path: &Path) -> ToolchainError {
    match error {
        ToolchainError::Unreadable { reason, .. } => ToolchainError::Unreadable {
            path: path.to_path_buf(),
            reason,
        },
        other => other,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The suffix the machine running these tests writes executables with.
    const EXECUTABLE_SUFFIX: &str = std::env::consts::EXE_SUFFIX;

    fn runtime(target: &str, frontend: Frontend) -> ToolchainComponent {
        ToolchainComponent::Runtime {
            target: zup_core::TargetTriple::parse(target).expect("valid target"),
            frontend,
        }
    }

    fn write_component(root: &Path, component: &ToolchainComponent, version: &str) -> PathBuf {
        let path = root.join(file_name(component, EXECUTABLE_SUFFIX));
        std::fs::write(&path, b"component bytes").expect("write component");
        let descriptor = ComponentDescriptor::of(component, version, &path).expect("describe");
        std::fs::write(
            root.join(descriptor_file_name(component, EXECUTABLE_SUFFIX)),
            descriptor.encode(),
        )
        .expect("write descriptor");
        path
    }

    #[test]
    fn a_component_is_accepted_only_for_the_release_that_produced_it() {
        let root = tempfile::TempDir::new().expect("temp dir");
        let component = runtime("x86_64-pc-windows-msvc", Frontend::Gui);
        let path = write_component(root.path(), &component, "0.1.0");
        assert!(read(&path, &component, "0.1.0").is_ok());
        let error = read(&path, &component, "0.2.0")
            .expect_err("a runtime from another release is refused")
            .to_string();
        assert!(error.contains("0.1.0"), "{error}");
        assert!(error.contains("0.2.0"), "{error}");
    }

    #[test]
    fn a_component_is_accepted_only_for_its_own_target_and_frontend() {
        let root = tempfile::TempDir::new().expect("temp dir");
        let component = runtime("x86_64-pc-windows-msvc", Frontend::Gui);
        let path = write_component(root.path(), &component, "0.1.0");
        let wrong_frontend = runtime("x86_64-pc-windows-msvc", Frontend::Console);
        let error = read(&path, &wrong_frontend, "0.1.0")
            .expect_err("a console template is not a gui template")
            .to_string();
        assert!(error.contains("presents"), "{error}");
        let wrong_target = runtime("aarch64-pc-windows-msvc", Frontend::Gui);
        let error = read(&path, &wrong_target, "0.1.0")
            .expect_err("an aarch64 runtime is not an x64 runtime")
            .to_string();
        assert!(error.contains("aarch64"), "{error}");
    }

    #[test]
    fn replaced_bytes_are_detected_even_when_the_descriptor_is_untouched() {
        let root = tempfile::TempDir::new().expect("temp dir");
        let component = runtime("x86_64-pc-windows-msvc", Frontend::Headless);
        let path = write_component(root.path(), &component, "0.1.0");
        // The same length, so the size check passes and the digest check is the
        // one that has to catch it. A replacement that also changed the size would
        // be caught by the cheaper check, and the test would prove nothing about
        // the digest.
        std::fs::write(&path, b"COMPONENT bytes").expect("replace");
        let error = read(&path, &component, "0.1.0")
            .expect_err("a component that is not the one described is refused")
            .to_string();
        assert!(error.contains("digest"), "{error}");
    }

    #[test]
    fn a_dispatcher_names_its_launcher_experience_rather_than_a_frontend() {
        let root = tempfile::TempDir::new().expect("temp dir");
        let gui = ToolchainComponent::Dispatcher {
            subsystem: Subsystem::Gui,
            online: false,
        };
        let console = ToolchainComponent::Dispatcher {
            subsystem: Subsystem::Console,
            online: false,
        };
        let path = write_component(root.path(), &gui, "0.1.0");
        assert!(read(&path, &gui, "0.1.0").is_ok());
        let error = read(&path, &console, "0.1.0")
            .expect_err("a gui launcher is not a console launcher")
            .to_string();
        assert!(error.contains("launcher"), "{error}");
    }

    #[test]
    fn a_launcher_without_the_network_path_is_not_a_launcher_with_it() {
        // Both are the same source and the same subsystem. Only one can fetch a
        // release, and an offline one composed into a thin artifact would refuse
        // on a user's machine rather than at build time.
        let root = tempfile::TempDir::new().expect("temp dir");
        let offline = ToolchainComponent::Dispatcher {
            subsystem: Subsystem::Gui,
            online: false,
        };
        let online = ToolchainComponent::Dispatcher {
            subsystem: Subsystem::Gui,
            online: true,
        };
        let path = write_component(root.path(), &offline, "0.1.0");
        assert!(read(&path, &offline, "0.1.0").is_ok());
        let error = read(&path, &online, "0.1.0")
            .expect_err("an offline launcher is not an online one")
            .to_string();
        assert!(error.contains("offline"), "{error}");
        assert!(error.contains("online"), "{error}");
    }

    #[test]
    fn a_descriptor_from_an_unknown_format_is_refused_rather_than_half_read() {
        let component = runtime("x86_64-pc-windows-msvc", Frontend::Gui);
        let descriptor = ComponentDescriptor {
            format_version: FORMAT_VERSION + 1,
            kind: ComponentKind::Runtime,
            zup_version: "0.1.0".to_owned(),
            target: "x86_64-pc-windows-msvc".to_owned(),
            frontend: Some("gui".to_owned()),
            subsystem: None,
            online: None,
            digest: hex(&[0; 32]),
            size: 1,
        };
        let error = descriptor
            .check(&component, "0.1.0")
            .expect_err("an unknown contract is refused")
            .to_string();
        assert!(error.contains("format"), "{error}");
    }

    #[test]
    fn the_component_file_name_carries_the_machine_it_is_for() {
        let name = file_name(
            &runtime("aarch64-pc-windows-msvc", Frontend::Console),
            EXECUTABLE_SUFFIX,
        );
        assert!(name.contains("aarch64-pc-windows-msvc"), "{name}");
        assert!(name.contains("console"), "{name}");
        assert!(name.starts_with("zup-setup-"), "{name}");
        let dispatcher = file_name(
            &ToolchainComponent::Dispatcher {
                subsystem: Subsystem::Gui,
                online: false,
            },
            EXECUTABLE_SUFFIX,
        );
        assert!(dispatcher.contains("i686-pc-windows-msvc"), "{dispatcher}");
    }

    /// The descriptor is found beside the file, so the two names have to be the
    /// same name. This is the mismatch that once produced "no descriptor" for a
    /// component that had one.
    #[test]
    fn a_descriptor_is_named_after_the_component_beside_it() {
        let component = runtime("x86_64-pc-windows-msvc", Frontend::Gui);
        let root = tempfile::TempDir::new().expect("temp dir");
        let path = write_component(root.path(), &component, "0.1.0");
        let descriptor = root
            .path()
            .join(descriptor_file_name(&component, EXECUTABLE_SUFFIX));
        assert!(descriptor.is_file(), "{}", descriptor.display());
        assert_eq!(
            descriptor
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.replace(DESCRIPTOR_SUFFIX, "")),
            path.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned),
            "the resolver looks for the component's own name plus the suffix"
        );
        assert!(read(&path, &component, "0.1.0").is_ok());
    }
}

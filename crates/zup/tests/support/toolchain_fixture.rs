//! Writing a toolchain component a test can hand to `zup build`.
//!
//! The real components are compiled by `cargo xtask toolchain build` and carry a
//! descriptor. A test that needs a component for a machine, a frontend, or a
//! launcher experience it cannot have a real one for writes the same shape here,
//! which is what keeps the test exercising the production check rather than a
//! bypass of it.
//!
//! `build` still works without any of this: the resolver finds the staged
//! toolchain. This module is for the cases where a test deliberately needs a
//! component that is *wrong*, which is a test of the refusal rather than of the
//! happy path, and for the components a contributor's machine cannot build
//! because they are for a machine it is not.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use zup_core::TargetTriple;
use zup_toolchain::{ComponentDescriptor, DESCRIPTOR_SUFFIX, Subsystem, ToolchainComponent};

/// The component a test asked for.
pub struct Staged {
    pub path: PathBuf,
    pub component: ToolchainComponent,
    /// Bytes to write instead of a synthesized PE image.
    ///
    /// The preset component is a `.zupui`, not a portable executable, so its
    /// fixture is a real package rather than a header: a test that staged
    /// something the resolver could not verify would be testing a bypass.
    pub bytes: Option<Vec<u8>>,
}

impl Staged {
    /// Write this component, and its descriptor, into `directory`.
    pub fn write(&self, directory: &Path) -> PathBuf {
        std::fs::create_dir_all(directory).expect("component directory");
        // The component's own target suffix, never the writing host's: a
        // Linux fixture written on Windows is extensionless, exactly as the
        // staged toolchain names it, because the resolver searches for that
        // name.
        let name = zup_toolchain::file_name(&self.component, component_suffix(&self.component));
        let path = directory.join(&name);
        let bytes = match &self.bytes {
            Some(bytes) => bytes.clone(),
            None => image(&self.component),
        };
        std::fs::write(&path, bytes).expect("component image");
        let descriptor = ComponentDescriptor::of(&self.component, ZUP_VERSION, &path)
            .expect("describe the component");
        std::fs::write(
            directory.join(format!("{name}{DESCRIPTOR_SUFFIX}")),
            descriptor.encode(),
        )
        .expect("component descriptor");
        path
    }
}

/// A runtime template for one target and frontend.
pub fn runtime(target: &str, frontend: zup_core::Frontend) -> Staged {
    Staged {
        path: PathBuf::new(),
        component: ToolchainComponent::Runtime {
            target: TargetTriple::parse(target).expect("a valid target triple"),
            frontend,
        },
        bytes: None,
    }
}

/// A launcher for one presentation experience.
pub fn dispatcher(subsystem: Subsystem, online: bool) -> Staged {
    Staged {
        path: PathBuf::new(),
        component: ToolchainComponent::Dispatcher { subsystem, online },
        bytes: None,
    }
}

/// The preset package a GUI build resolves when the application named none.
///
/// One package for every target, because that is what a `.zupui` is: the build
/// reads it and picks the target it needs. A GUI build that has no preset staged
/// refuses, so a test that stages a GUI target stages this too.
pub fn preset(targets: &[&str]) -> Staged {
    Staged {
        path: PathBuf::new(),
        component: ToolchainComponent::Preset,
        bytes: Some(package(targets)),
    }
}

/// The zup version the tests run against.
///
/// The integration tests share the crate's version because a component stamped
/// with any other version is refused by design, and a test that wanted to prove
/// the refusal has a test for that.
const ZUP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The suffix a fixture component is stored under: its own target's, never the
/// writing host's.
fn component_suffix(component: &ToolchainComponent) -> &'static str {
    match component {
        ToolchainComponent::Runtime { target, .. } => target.executable_suffix(),
        ToolchainComponent::Dispatcher { .. } => ".exe",
        ToolchainComponent::Preset => "",
    }
}

/// A minimal image header that agrees with what the descriptor claims.
///
/// The header is the only independent statement a build host can make about a
/// component it cannot run, so the fixture has to say the same thing the
/// descriptor does - otherwise the two disagree and the resolver refuses the
/// component for a reason the test never intended.
///
/// A Windows target gets a PE with the machine and subsystem the component
/// names. A Linux target gets a header-only ELF with the machine the target
/// names: enough for the architecture identity the resolver's header check
/// reads from an ELF (an ELF records no subsystem, so the frontend comes from
/// the descriptor alone, exactly as in production).
fn image(component: &ToolchainComponent) -> Vec<u8> {
    if let ToolchainComponent::Runtime { target, .. } = component
        && target.operating_system() == zup_core::TargetOperatingSystem::Linux
    {
        return elf_image(target.as_str());
    }
    let (machine, subsystem): (u16, u16) = match component {
        ToolchainComponent::Runtime { target, frontend } => (
            machine_of(target.as_str()),
            match frontend {
                zup_core::Frontend::Gui => 2,
                zup_core::Frontend::Console | zup_core::Frontend::Headless => 3,
            },
        ),
        ToolchainComponent::Dispatcher { subsystem, .. } => {
            (0x014c, if *subsystem == Subsystem::Gui { 2 } else { 3 })
        }
        ToolchainComponent::Preset => unreachable!("a package is not a PE image"),
    };
    let mut bytes = vec![0u8; 0x178];
    bytes[..2].copy_from_slice(b"MZ");
    bytes[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
    bytes[0x40..0x44].copy_from_slice(b"PE\0\0");
    bytes[0x44..0x46].copy_from_slice(&machine.to_le_bytes());
    bytes[0x46..0x48].copy_from_slice(&1u16.to_le_bytes());
    bytes[0x54..0x56].copy_from_slice(&240u16.to_le_bytes());
    bytes[0x58..0x5a].copy_from_slice(&0x20bu16.to_le_bytes());
    bytes[0x9c..0x9e].copy_from_slice(&subsystem.to_le_bytes());
    bytes
}

/// The COFF machine value a target triple's architecture names.
fn machine_of(target: &str) -> u16 {
    if target.starts_with("aarch64") {
        0xaa64
    } else if target.starts_with("i686") {
        0x014c
    } else {
        0x8664
    }
}

/// A header-only ELF image for a Linux target: magic, class, data, version,
/// type, and machine. No program or section tables - there is no content to
/// describe, and the resolver reads only the architecture identity.
fn elf_image(target: &str) -> Vec<u8> {
    let machine: u16 = if target.starts_with("aarch64") {
        183
    } else {
        62
    };
    let mut bytes = vec![0u8; 64];
    bytes[..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2; // 64-bit
    bytes[5] = 1; // little-endian
    bytes[6] = 1; // version
    bytes[16..18].copy_from_slice(&2u16.to_le_bytes()); // executable
    bytes[18..20].copy_from_slice(&machine.to_le_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_le_bytes()); // version
    bytes[52..54].copy_from_slice(&64u16.to_le_bytes()); // header size
    bytes
}

/// A file that is not a PE image at all.
pub fn not_an_image() -> Vec<u8> {
    b"not a PE".to_vec()
}

/// A real preset package carrying one binary per named target.
///
/// The same writer `zup preset pack` uses, because a fixture that hand-rolled a
/// package would be a test of the fixture rather than of the path a build takes.
pub fn package(targets: &[&str]) -> Vec<u8> {
    let mut writer = zup_artifact::preset::PresetPackageWriter::new(
        zup_preset_protocol::PresetDescription::new("aurora", "1.0.0", settings_schema()),
    )
    .expect("a valid description");
    for target in targets {
        writer
            .add_binary(
                TargetTriple::parse(target).expect("a valid target triple"),
                format!("native preset for {target}")
                    .repeat(64)
                    .into_bytes(),
            )
            .expect("one binary per target");
    }
    writer.finish().expect("a verified package")
}

/// A settings schema with the shape a real preset generates, including one
/// setting that names a file the application provides.
pub fn settings_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "hero": { "type": ["string", "null"] },
            "accent": { "type": ["string", "null"] },
            "logo": {
                "anyOf": [
                    { "$ref": "#/$defs/AssetRef" },
                    { "type": "null" }
                ]
            }
        },
        "$defs": {
            "AssetRef": {
                "type": "string",
                "title": "Asset",
                "x-zup-asset": true
            }
        }
    })
}

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
}

impl Staged {
    /// Write this component, and its descriptor, into `directory`.
    pub fn write(&self, directory: &Path) -> PathBuf {
        std::fs::create_dir_all(directory).expect("component directory");
        let name = zup_toolchain::file_name(&self.component, EXECUTABLE_SUFFIX);
        let path = directory.join(&name);
        std::fs::write(&path, image(&self.component)).expect("component image");
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
    }
}

/// A launcher for one presentation experience.
pub fn dispatcher(subsystem: Subsystem, online: bool) -> Staged {
    Staged {
        path: PathBuf::new(),
        component: ToolchainComponent::Dispatcher { subsystem, online },
    }
}

/// The zup version the tests run against.
///
/// The integration tests share the crate's version because a component stamped
/// with any other version is refused by design, and a test that wanted to prove
/// the refusal has a test for that.
const ZUP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The suffix this machine writes executables with.
const EXECUTABLE_SUFFIX: &str = std::env::consts::EXE_SUFFIX;

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

/// A minimal PE header that agrees with what the descriptor claims.
///
/// The header is the only independent statement a build host can make about a
/// component it cannot run, so the fixture has to say the same thing the
/// descriptor does — otherwise the two disagree and the resolver refuses the
/// component for a reason the test never intended.
fn image(component: &ToolchainComponent) -> Vec<u8> {
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

/// A file that is not a PE image at all.
pub fn not_an_image() -> Vec<u8> {
    b"not a PE".to_vec()
}

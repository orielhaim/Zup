//! Linux backend for zup.
//!
//! A sibling of `zup-windows`, not a layer above it and not a copy of it. Each
//! backend owns the mechanisms of exactly one operating system, and the portable
//! crates below them own everything else.
//!
//! What is here in this phase is the part of a Linux backend that is
//! well-defined independently of a working installer: what this host is, where a
//! Linux target's paths land, where a scope's persistent state belongs, and what
//! a build source is allowed to be. What is deliberately absent is the part that
//! depends on work not done yet - desktop integration, services, privilege
//! separation, a worker transport. An absent concept is reported as absent at
//! the capability boundary rather than answered with a function that returns
//! `Unsupported`.
//!
//! Every Linux-specific idea in this crate is a *lowering* of a portable one:
//! [`host`] answers zup's existing selection model with this machine's identity,
//! [`lowering`] turns a portable [`TargetPath`] into a host path, [`state`] places
//! a scope's state where the XDG model puts it, and [`source_policy`] decides
//! what a build may read. None of them introduces a Linux-shaped type to the
//! portable model, which is what lets both backends be used by the same caller.
//!
//! ```text
//! portable intent
//!        ↓
//! native lowering      ← this crate
//!        ↓
//! native mechanism
//! ```
//!
//! # This crate is Linux-only
//!
//! Every module is gated on the target operating system, so on any other host the
//! crate compiles to nothing and exports nothing. That is deliberate rather than a
//! limitation: a backend that answers on a platform it has no mechanisms for would
//! be a backend whose answers are guesses, and the honest thing for a host without
//! this backend is to have no symbols to call. The package matrix records the same
//! fact - `zup-linux` is verified where Linux can be built - so a CI job that wants
//! to prove anything about this crate has to be a Linux job.

#![forbid(unsafe_code)]

#[cfg(target_os = "linux")]
mod host;
#[cfg(target_os = "linux")]
mod lowering;
#[cfg(target_os = "linux")]
mod source_policy;
#[cfg(target_os = "linux")]
mod state;

#[cfg(target_os = "linux")]
pub use host::{
    HostError, additional_architectures, host_execution, host_version, native_architecture,
};
#[cfg(target_os = "linux")]
pub use lowering::{
    LinuxPathLoweringError, linux_target_path, target_path_from_host, to_host_path,
};
#[cfg(target_os = "linux")]
pub use source_policy::{LinuxSourceFilePolicy, SourceEntryKind};
#[cfg(target_os = "linux")]
pub use state::{LinuxStateError, machine_state_root, state_root, user_state_root};

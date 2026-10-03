//! Bindings for the plugin world, as the host links against them.
//!
//! Generated from the canonical WIT the ABI crate owns rather than from a copy
//! here. There is one contract and both sides generate from it: a host that
//! validated against a different WIT than a guest was built against would accept
//! components it cannot call, and a copy in this crate is a place that copy
//! could stop being true.
//!
//! The invocation is written by `build.rs`, which reads the location from the
//! ABI crate, because the macro takes a literal path and nothing else.
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
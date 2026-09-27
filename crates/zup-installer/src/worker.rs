//! The elevated worker.
//!
//! Machine-scope work runs in a separate, elevated process so that the unelevated
//! process a person launched — the window, the console — never holds an elevated
//! handle to anything. That split is a security boundary, and this module is the
//! worker half of it.
//!
//! Everything the worker trusts arrives in one argument and is checked before any
//! side effect: the protocol version, the session identifier, the pipe name, the
//! parent process and session, the target, and the hash of the plan. A worker
//! that cannot prove who started it and what it was asked to do connects to
//! nothing and writes nothing.

/// Run the worker for one authenticated session.
///
/// The bootstrap is a single opaque string rather than a set of flags because it
/// is passed as one value on a command line the parent constructed, and a
/// protocol that is a protocol — not a set of independently-settable options — is
/// one that cannot be half-configured.
pub fn run(bootstrap: &str) -> miette::Result<()> {
    let bootstrap = zup_windows::parse_bootstrap(bootstrap)
        .map_err(|error| miette::miette!("worker bootstrap rejected: {error}"))?;
    if bootstrap.expected_parent_pid == 0 {
        return Err(miette::miette!(
            "worker bootstrap rejected: zero parent pid"
        ));
    }
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| miette::miette!("worker runtime: {error}"))?;
    let cancel = tokio_util::sync::CancellationToken::new();
    tokio
        .block_on(zup_windows::run_worker(bootstrap, cancel))
        .map(|_| ())
        .map_err(|error| miette::miette!("worker failed: {error}"))
}

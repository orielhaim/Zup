#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bootstrap = std::env::args().nth(1).ok_or("missing bootstrap")?;
    let bootstrap = zup_windows::parse_bootstrap(&bootstrap)?;
    let scope = match std::env::args().nth(2).as_deref() {
        Some("user") => zup_core::SelectedScope::User,
        Some("machine") => zup_core::SelectedScope::Machine,
        _ => return Err("missing worker authority".into()),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(zup_windows::run_worker_for_test(
        bootstrap,
        tokio_util::sync::CancellationToken::new(),
        scope,
    ))?;
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    panic!("zup-test-worker runs on Windows only");
}

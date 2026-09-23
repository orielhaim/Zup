fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bootstrap = std::env::args().nth(1).ok_or("missing bootstrap")?;
    let bootstrap = zup_windows::parse_bootstrap(&bootstrap)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(zup_windows::run_worker_for_test(
        bootstrap,
        tokio_util::sync::CancellationToken::new(),
    ))?;
    Ok(())
}

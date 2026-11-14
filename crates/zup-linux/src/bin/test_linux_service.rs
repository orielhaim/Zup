#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments
        .first()
        .is_some_and(|argument| argument == "--version")
    {
        println!("{}", option_env!("ZUP_TEST_SERVICE_VERSION").unwrap_or("1"));
        return Ok(());
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "--detached-child")
    {
        rustix::process::setsid()?;
        std::fs::write(&arguments[1], std::process::id().to_string())?;
        std::future::pending::<()>().await;
    }
    if let Some(path) = arguments
        .windows(2)
        .find(|pair| pair[0] == "--detached-child-file")
        .map(|pair| &pair[1])
    {
        std::process::Command::new(std::env::current_exe()?)
            .args(["--detached-child", path])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
    }
    let value = |name: &str| -> Result<Option<u64>, Box<dyn std::error::Error>> {
        arguments
            .windows(2)
            .find(|pair| pair[0] == name)
            .map(|pair| pair[1].parse().map_err(Into::into))
            .transpose()
    };
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let startup = async {
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_millis(value("--ready-after-ms")?.unwrap_or(0));
        while tokio::time::Instant::now() < deadline {
            if arguments
                .iter()
                .any(|argument| argument == "--extend-start")
            {
                sd_notify::notify(&[sd_notify::NotifyState::ExtendTimeoutUsec(1_000_000)])?;
            }
            tokio::time::sleep_until(
                (tokio::time::Instant::now() + std::time::Duration::from_millis(100)).min(deadline),
            )
            .await;
        }
        if arguments.iter().any(|argument| argument == "--notify") {
            sd_notify::notify(&[sd_notify::NotifyState::Ready])?;
        }
        if let Some(delay) = value("--fail-after-ms")? {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            std::process::exit(1);
        }
        std::future::pending::<Result<(), Box<dyn std::error::Error>>>().await
    };
    let shutdown = async {
        terminate.recv().await;
        if arguments.iter().any(|argument| argument == "--ignore-term") {
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(
            value("--stop-after-ms")?.unwrap_or(0),
        ))
        .await;
        if let Some(path) = arguments
            .windows(2)
            .find(|pair| pair[0] == "--stopped-file")
            .map(|pair| &pair[1])
        {
            std::fs::write(path, std::process::id().to_string())?;
        }
        Ok(())
    };
    tokio::select! {
        result = startup => result,
        result = shutdown => result,
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    panic!("the Linux service fixture requires Linux");
}

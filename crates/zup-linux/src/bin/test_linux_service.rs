#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments
        .first()
        .is_some_and(|argument| argument == "--version")
    {
        println!("{}", option_env!("ZUP_TEST_SERVICE_VERSION").unwrap_or("1"));
        return Ok(());
    }
    let value = |name: &str| -> Result<Option<u64>, Box<dyn std::error::Error>> {
        arguments
            .windows(2)
            .find(|pair| pair[0] == name)
            .map(|pair| pair[1].parse().map_err(Into::into))
            .transpose()
    };
    if let Some(delay) = value("--ready-after-ms")? {
        std::thread::sleep(std::time::Duration::from_millis(delay));
    }
    if arguments.iter().any(|argument| argument == "--notify") {
        sd_notify::notify(&[sd_notify::NotifyState::Ready])?;
    }
    if let Some(delay) = value("--fail-after-ms")? {
        std::thread::sleep(std::time::Duration::from_millis(delay));
        std::process::exit(1);
    }
    loop {
        std::thread::park();
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    panic!("the Linux service fixture requires Linux");
}

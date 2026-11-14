fn main() {
    let args: Vec<_> = std::env::args().collect();
    if let Some(ready) = args.windows(2).find(|pair| pair[0] == "--ready") {
        std::fs::write(&ready[1], b"running").unwrap();
    }
    if let Some(release) = args.windows(2).find(|pair| pair[0] == "--release") {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !std::path::Path::new(&release[1]).exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    let marker = args.windows(2).find(|pair| pair[0] == "--marker").unwrap();
    std::fs::write(&marker[1], b"authenticated").unwrap();
    let code = args.windows(2).find(|pair| pair[0] == "--exit")
        .map(|pair| pair[1].parse().unwrap()).unwrap_or(0);
    std::process::exit(code);
}

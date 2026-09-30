fn main() {
    if let Err(error) = zup_ui_sdk::run::<aurora::Aurora>() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

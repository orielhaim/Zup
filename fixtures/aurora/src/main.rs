fn main() {
    if let Err(error) = zup_sdk::preset::run::<aurora::Aurora>() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn main() {
    if let Err(error) = zup_sdk::preset::run::<zup_preset_default::DefaultPreset>() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

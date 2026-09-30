fn main() {
    if let Err(error) = zup_ui_sdk::run::<zup_preset_default::DefaultPreset>() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

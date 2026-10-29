fn main() {
    if std::env::var_os("ZUP_TEST_UPDATE").is_some() {
        zup_preset_ipc::test_support::update();
    } else {
        zup_preset_ipc::test_support::install();
    }
}

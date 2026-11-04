#![windows_subsystem = "windows"]

fn main() {
    // Developer flag: accept injected input and keep running unfocused.
    let test_mode = std::env::args().any(|a| a == "--test-mode");
    prc::ui::run(test_mode);
}

use voicekeyboard::core::hotkey;

fn main() {
    for spec in ["alt+KeyR", "alt+KeyE", "ctrl+Shift+KeyS", "ctrl+alt+KeyJ"] {
        match hotkey::parse(spec) {
            Ok(hk) => println!("{spec} -> id {} ({hk})", hk.id()),
            Err(e) => println!("{spec} -> error {e}"),
        }
    }
}

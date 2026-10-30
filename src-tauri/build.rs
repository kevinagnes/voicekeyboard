fn main() {
    tauri_build::build();

    // Test/example binaries link Tauri's tray code, which imports
    // TaskDialogIndirect from comctl32 v6. The app binary gets a manifest from
    // tauri-build; these targets need one as well or Windows resolves comctl32
    // v5 and the process fails at startup.
    #[cfg(target_os = "windows")]
    {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("test.manifest");
        println!("cargo:rustc-link-arg-examples=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-examples=/MANIFESTINPUT:{}",
            manifest.display()
        );
        println!("cargo:rerun-if-changed=test.manifest");
    }
}

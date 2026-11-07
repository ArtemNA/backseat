fn main() {
    // The window, tray and exe icon; tauri-build alone did not rebuild when only it changed.
    println!("cargo:rerun-if-changed=icons/icon.ico");
    tauri_build::build()
}

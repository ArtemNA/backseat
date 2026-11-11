// The exe's icon (resource 1): Windows shows it for the file, its shortcuts, the taskbar and
// backseat:// links; GPUI loads resource 1 for the window.
fn main() {
    println!("cargo:rerun-if-changed=../app/icons/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = tauri_winres::WindowsResource::new();
        res.set_icon("../app/icons/icon.ico");
        res.set("ProductName", "Backseat");
        res.set("FileDescription", "Backseat");
        res.compile().expect("Windows resources");
    }
}

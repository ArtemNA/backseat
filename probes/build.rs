fn main() {
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        // The D3D11 helper library has no Rust bindings, so link it directly on Windows.
        Ok("windows") => {
            pkg_config::Config::new()
                .atleast_version("1.28")
                .probe("gstreamer-d3d11-1.0")
                .expect("gstreamer-d3d11-1.0 not found through pkg-config");
        }
        // The official framework's libraries use @rpath, so the binary must name it.
        Ok("macos") => {
            println!("cargo:rustc-link-arg=-Wl,-rpath,/Library/Frameworks/GStreamer.framework/Versions/1.0/lib");
        }
        _ => {}
    }
}

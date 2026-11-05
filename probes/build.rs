fn main() {
    // The D3D11 helper library has no Rust bindings, so link it directly.
    pkg_config::Config::new()
        .atleast_version("1.28")
        .probe("gstreamer-d3d11-1.0")
        .expect("gstreamer-d3d11-1.0 not found through pkg-config");
}

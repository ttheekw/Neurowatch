fn main() {
    // Build dependencies run on the host, so cfg(windows) would miss cross builds.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=windows.rc");
    println!("cargo:rerun-if-changed=../../ui/neurowatch/resources/icons/neurowatch.ico");
    embed_resource::compile_for("windows.rc", ["nw-backend"], embed_resource::NONE);
}

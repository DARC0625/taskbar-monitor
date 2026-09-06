fn main() {
    for path in [
        "assets/app.rc",
        "assets/app.manifest",
        "assets/taskbar-monitor.ico",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let numeric = format!("{},0", version.replace('.', ","));
    embed_resource::compile(
        "assets/app.rc",
        [
            format!("APP_VERSION={numeric}"),
            format!("APP_VERSION_TEXT={version}"),
        ],
    )
    .manifest_required()
    .expect("Windows icon, version and manifest resources must compile");
}

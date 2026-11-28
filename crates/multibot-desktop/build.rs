fn main() {
    println!("cargo:rerun-if-changed=assets/multibot.rc");
    println!("cargo:rerun-if-changed=assets/multibot.ico");

    // Build scripts run on the host, including when cross-compiling for Windows.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile(
            "assets/multibot.rc",
            embed_resource::ParamsIncludeDirs(["assets"]),
        )
        .manifest_required()
        .expect("failed to embed the Multibot Windows icon");
    }
}

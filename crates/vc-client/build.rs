//! Every target: export the target triple as `VC_TARGET` for the updater.
//!
//! Windows: embed the app icon (and file version info) in the exe, so Explorer,
//! taskbar pins and shortcuts show it. The window icon is set separately at runtime
//! (`main.rs`). `assets/icon.ico` is made from the iOS `AppIcon.png` with rounded
//! corners.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // The target triple the updater checks manifests against (`update::manifest::TARGET`).
    println!(
        "cargo:rustc-env=VC_TARGET={}",
        std::env::var("TARGET").expect("cargo sets TARGET for build scripts")
    );
    println!("cargo:rerun-if-changed=assets/icon.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico")
            .set("ProductName", "Voicechat")
            .set("FileDescription", "Voicechat");
        res.compile().expect("embed Windows resources (icon)");
    }
}

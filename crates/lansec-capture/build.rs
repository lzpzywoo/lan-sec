fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if os == "macos" {
        let vendor = root.join("../../vendor/vt");
        println!("cargo:rerun-if-changed={}", vendor.join("lansec_vt.m").display());
        cc::Build::new()
            .file(vendor.join("lansec_vt.m"))
            .include(&vendor)
            .flag("-fobjc-arc")
            .flag("-fmodules")
            .compile("lansec_vt");
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rustc-link-lib=framework=ScreenCaptureKit");
        println!("cargo:rustc-link-lib=framework=VideoToolbox");
        println!("cargo:rustc-link-lib=framework=CoreMedia");
        println!("cargo:rustc-link-lib=framework=CoreVideo");
        println!("cargo:rustc-link-lib=framework=CoreGraphics");
        println!("cargo:rustc-link-lib=framework=ApplicationServices");
        println!("cargo:rustc-link-lib=framework=IOSurface");
        println!("cargo:rustc-link-lib=framework=Metal");
        println!("cargo:rustc-link-lib=framework=QuartzCore");
        println!("cargo:rustc-link-lib=framework=AppKit");
        println!("cargo:rustc-link-lib=framework=CoreImage");
    }
}

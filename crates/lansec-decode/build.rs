fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let vendor = root.join("../../vendor/mfx");
        println!("cargo:rerun-if-changed={}", vendor.join("lansec_mfx.c").display());
        println!("cargo:rerun-if-changed={}", vendor.join("lansec_mfx.h").display());
        cc::Build::new()
            .file(vendor.join("lansec_mfx.c"))
            .include(&vendor)
            .include(vendor.join("include"))
            .define("WIN32_LEAN_AND_MEAN", None)
            .define("COBJMACROS", None)
            .warnings(false)
            .compile("lansec_mfx");
        let dxva = root.join("../../vendor/d3d11hevc");
        println!("cargo:rerun-if-changed={}", dxva.join("lansec_hevc_dxva.c").display());
        cc::Build::new()
            .file(dxva.join("lansec_hevc_dxva.c"))
            .include(&dxva)
            .define("WIN32_LEAN_AND_MEAN", None)
            .define("COBJMACROS", None)
            .warnings(false)
            .compile("lansec_hevc_dxva");
        println!("cargo:rustc-link-lib=d3d11");
        println!("cargo:rustc-link-lib=ole32");
    }
}

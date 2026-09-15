fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let vendor = root.join("../../vendor/nvenc");
        println!("cargo:rerun-if-changed={}", vendor.join("lansec_nvenc.c").display());
        println!("cargo:rerun-if-changed={}", vendor.join("lansec_nvenc.h").display());
        cc::Build::new()
            .file(vendor.join("lansec_nvenc.c"))
            .include(&vendor)
            .define("WIN32_LEAN_AND_MEAN", None)
            .warnings(false)
            .compile("lansec_nvenc");
        println!("cargo:rustc-link-lib=d3d11");
        println!("cargo:rustc-link-lib=dxgi");
    }
}

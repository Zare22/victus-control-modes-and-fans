fn main() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("app.manifest");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());
    // The execution level comes from app.manifest.
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:NO");
    // Resolve static DLL imports only from System32 (LOAD_LIBRARY_SEARCH_SYSTEM32).
    println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
    if matches!(std::env::var("CARGO_CFG_TARGET_ARCH").as_deref(), Ok("x86_64" | "x86")) {
        println!("cargo:rustc-link-arg-bins=/CETCOMPAT");
    }
}

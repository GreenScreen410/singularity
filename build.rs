fn main() {
    // Embed the app icon + version resource into the Windows exe.
    // Uses windres (works with both MSVC and mingw cross builds).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/singularity.ico");
        res.set("ProductName", "Singularity");
        res.set("FileDescription", "A drifting black hole over your desktop");
        res.set("LegalCopyright", "MIT License");
        if let Err(e) = res.compile() {
            // Don't fail the build over a missing windres — just skip the icon.
            println!("cargo:warning=icon resource skipped: {e}");
        }
    }
}

fn main() {
    // the icon goes into the Windows exe so it isn't blank in Explorer and in
    // Apps & features; a build without the resource compiler still works
    println!("cargo:rerun-if-changed=assets/netcast.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/netcast.ico");
        res.set("CompanyName", "Londopy");
        res.set("LegalCopyright", "Copyright (c) 2026 Londopy");
        if let Err(e) = res.compile() {
            println!("cargo:warning=couldn't embed the exe icon: {e}");
        }
    }
}

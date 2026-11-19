//! Windows version information for continuityd.exe: the product name,
//! description, publisher and version that Explorer's Properties, Task
//! Manager and "Apps" show — and that antivirus heuristics weigh, an
//! unsigned program with none of it being one of the things that make
//! Windows Defender suspicious of one. Nothing happens for any other target.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    resource
        .set("ProductName", "Continuity")
        .set("FileDescription", "Continuity")
        .set("CompanyName", "hardope")
        .set("LegalCopyright", "Copyright (c) 2026 hardope. MIT License.")
        .set("OriginalFilename", "continuityd.exe")
        .set("InternalName", "continuityd")
        .set("Comments", "Clipboard sync, file sharing and remote control between nearby devices");
    // A build without it still works; it just looks less like a known
    // program. Said loudly, so CI's log shows it.
    if let Err(e) = resource.compile() {
        println!("cargo:warning=couldn't embed Windows version information: {e}");
    }
}

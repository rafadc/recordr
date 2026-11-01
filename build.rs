// Embed Info.plist so macOS shows the Audio Capture permission prompt for process taps.
fn main() {
    println!("cargo:rerun-if-changed=Info.plist");
    let plist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Info.plist");
    println!(
        "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        plist.display()
    );
}

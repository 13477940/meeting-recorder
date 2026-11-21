// screencapturekit crate 只找 Xcode 內的 Swift runtime；只裝 Command Line Tools 時補上 CLT 的路徑。
fn main() {
    let dev = std::process::Command::new("xcode-select")
        .arg("-p")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    for p in [
        format!("{dev}/usr/lib/swift/macosx"),
        format!("{dev}/Toolchains/XcodeDefault.xctoolchain/usr/lib/swift/macosx"),
    ] {
        if std::path::Path::new(&p).exists() {
            println!("cargo:rustc-link-search=native={p}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}

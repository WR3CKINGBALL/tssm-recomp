fn main() {
    // Newer Android devices use 16 KB memory pages and refuse libraries aligned for 4 KB.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-z,max-page-size=16384");
    }
}

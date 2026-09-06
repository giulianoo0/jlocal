fn main() {
    let cargo_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.1.0".into());
    let version = std::env::var("JLOCAL_VERSION").unwrap_or_else(|_| format!("v{cargo_version}"));
    println!("cargo:rustc-env=JLOCAL_VERSION={version}");
    println!("cargo:rerun-if-env-changed=JLOCAL_VERSION");
}

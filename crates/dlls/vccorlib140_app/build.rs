use std::env;
use std::path::PathBuf;

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
        let def_path = manifest_dir.join("vccorlib140_app.def");
        println!("cargo:rerun-if-changed={}", def_path.display());
        println!("cargo:rustc-link-arg={}", def_path.display());
    }
}

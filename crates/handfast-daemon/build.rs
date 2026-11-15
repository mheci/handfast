//! Exposes the build target triple, which the updater uses to pick an asset.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.is_empty() {
        println!("cargo:rustc-env=HANDFAST_TARGET={target}");
    }
}

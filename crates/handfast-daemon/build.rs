//! Exposes the build target triple, which the updater uses to pick an asset.

fn main() {
    let target = std::env::var("TARGET").expect("cargo sets TARGET");
    println!("cargo:rustc-env=HANDFAST_TARGET={target}");
}

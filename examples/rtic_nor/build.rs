//! Puts `memory.x` on the linker's search path, for `link.x` of cortex-m-rt.

fn main() {
    println!(
        "cargo:rustc-link-search={}",
        std::env::var("CARGO_MANIFEST_DIR").unwrap()
    );
    println!("cargo:rerun-if-changed=memory.x");
}

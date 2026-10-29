// No extra build dependency; compile the small host-only, plain-data ImageIO
// adapter for Cargo's target, not the ARM guest. Never install anything.
use std::{env, path::PathBuf, process::Command};
fn main() {
    let target = env::var("TARGET").unwrap();
    assert!(target == "x86_64-apple-darwin", "Attesor host adapter requires x86-64 macOS");
    let source = "../../native/imageio-buffer.c";
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let object = out.join("imageio-buffer.o");
    let archive = out.join("libattesor_imageio.a");
    assert!(Command::new("clang").args(["-target", &target, "-O2", "-Wall", "-Wextra", "-Werror", "-c", source, "-o"]).arg(&object).status().unwrap().success());
    assert!(Command::new("ar").arg("crs").arg(&archive).arg(&object).status().unwrap().success());
    println!("cargo:rerun-if-changed={source}");
    println!("cargo:rerun-if-changed=../../native/imageio-buffer.h");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=attesor_imageio");
    for framework in ["CoreFoundation", "CoreGraphics", "ImageIO"] { println!("cargo:rustc-link-lib=framework={framework}"); }
}

//! On Windows, builds the virtual camera DLL (the `vcam` crate) and exposes its
//! path so the exe can embed it and install it on first run.
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    for path in ["vcam/src", "vcam/Cargo.toml", "vcam/Cargo.lock", "shared/src", "shared/Cargo.toml"] {
        println!("cargo:rerun-if-changed={path}");
    }
    let target = std::env::var("TARGET").unwrap();
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("vcam");
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "--release", "--locked", "--manifest-path", "vcam/Cargo.toml", "--target", &target])
        .arg("--target-dir")
        .arg(&out)
        // Flags meant for this crate must not leak into the nested build.
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_TARGET_DIR")
        .status()
        .expect("failed to run cargo for the vcam crate");
    assert!(status.success(), "building the virtual camera DLL failed");
    let dll = out.join(&target).join("release").join("matrixcam_vcam.dll");
    assert!(dll.exists(), "expected {} to exist", dll.display());
    println!("cargo:rustc-env=MATRIXCAM_VCAM_DLL={}", dll.display());
}

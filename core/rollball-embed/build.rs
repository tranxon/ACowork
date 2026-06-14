//! Build script for rollball-embed.
//!
//! ORT linking is configured via the `ORT_LIB_LOCATION` environment variable,
//! which is auto-detected by:
//!   - `dev/ort_env.js`         (used by `npm run tauri dev`)
//!   - `dev/build_core.ps1`     (Windows)
//!   - `dev/build_core.sh`      (Linux / macOS)
//!   - `dev/setup_ort.ps1/sh`   (initial ORT download)
//!
//! If `ORT_LIB_LOCATION` is unset and `download-ort` is not active, this
//! script emits a warning so developers know to install ORT first.

fn main() {
    println!("cargo:rerun-if-env-changed=ORT_LIB_LOCATION");
    println!("cargo:rerun-if-env-changed=ORT_DYLIB_PATH");
    println!("cargo:rerun-if-env-changed=ORT_PREFER_DYNAMIC_LINK");

    if std::env::var("ORT_LIB_LOCATION").is_ok() {
        return;
    }

    if cfg!(feature = "download-ort") {
        return;
    }

    // ORT not configured — ort-sys will print its own detailed error
    // during the link step. Just emit a heads-up.
    println!(
        "cargo:warning=ORT_LIB_LOCATION not set and download-ort not enabled. \
         Run dev/setup_ort.ps1 (Windows) or dev/setup_ort.sh (Linux/macOS) to install ONNX Runtime."
    );
}

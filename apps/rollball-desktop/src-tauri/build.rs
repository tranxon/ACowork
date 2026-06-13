//! Build script for the RollBall desktop app.
//!
//! Before invoking `tauri_build::build()`, this script copies the core
//! workspace binaries (gateway, runtime, embed) and the ONNX runtime DLL
//! into a `bin/` staging directory inside `src-tauri/`. This allows
//! `tauri.conf.json` to reference a fixed local path instead of fragile
//! `target/{profile}/` glob patterns that break on a fresh clone.
//!
//! The `beforeDevCommand` / `beforeBuildCommand` in `tauri.conf.json` are
//! responsible for building the core workspace first, so the binaries
//! already exist in `target/{profile}/` by the time this script runs.

use std::path::PathBuf;

/// Binaries to copy from the workspace target directory.
const BINARIES: &[&str] = &["rollball-gateway", "rollball-runtime", "rollball-embed"];

fn main() {
    // 1. Determine build profile and locate workspace target directory.
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    // manifest_dir = .../apps/rollball-desktop/src-tauri
    // Workspace root = .../ (3 levels up)
    let workspace_root = manifest_dir
        .parent() // apps/rollball-desktop
        .and_then(|p| p.parent()) // apps
        .and_then(|p| p.parent()) // workspace root
        .expect("Cannot determine workspace root from CARGO_MANIFEST_DIR");

    let target_dir = workspace_root.join("target").join(&profile);
    let bin_dir = manifest_dir.join("bin");

    // 2. Create the staging directory.
    std::fs::create_dir_all(&bin_dir).expect("Failed to create bin/ staging directory");

    let exe_ext = if cfg!(windows) { ".exe" } else { "" };

    // 3. Copy each binary.
    for &name in BINARIES {
        let src = target_dir.join(format!("{name}{exe_ext}"));
        let dst = bin_dir.join(format!("{name}{exe_ext}"));
        if src.exists() {
            std::fs::copy(&src, &dst).unwrap_or_else(|e| {
                panic!("Failed to copy {}: {}", src.display(), e);
            });
            println!("cargo:warning=Copied {name} ({profile}) to bin/");
        } else {
            println!(
                "cargo:warning=Binary not found: {} (run `cd core && cargo build -p {name}` first)",
                src.display()
            );
        }
    }

    // 4. Copy ONNX runtime shared library (platform-specific).
    if cfg!(windows) {
        let dll_src = target_dir.join("onnxruntime.dll");
        let dll_dst = bin_dir.join("onnxruntime.dll");
        if dll_src.exists() {
            std::fs::copy(&dll_src, &dll_dst).unwrap_or_else(|e| {
                panic!("Failed to copy onnxruntime.dll: {}", e);
            });
            println!("cargo:warning=Copied onnxruntime.dll to bin/");
        } else {
            println!(
                "cargo:warning=onnxruntime.dll not found in {} (will be downloaded by ort crate on first build)",
                target_dir.display()
            );
        }
    } else if cfg!(target_os = "macos") {
        for lib_name in &["libonnxruntime.dylib"] {
            let src = target_dir.join(lib_name);
            let dst = bin_dir.join(lib_name);
            if src.exists() {
                let _ = std::fs::copy(&src, &dst);
            }
        }
    } else {
        // Linux
        for lib_name in &["libonnxruntime.so"] {
            let src = target_dir.join(lib_name);
            let dst = bin_dir.join(lib_name);
            if src.exists() {
                let _ = std::fs::copy(&src, &dst);
            }
        }
    }

    // 5. Copy LSP config and install scripts to bin/ for Gateway LSP support.
    //
    // NOTE: We do NOT copy embedding_models.json here. The Tauri build.rs is
    // not the right place — the Desktop App may link to a remote Gateway and
    // a local copy would be dead weight. Instead, the source file is listed
    // directly in tauri.conf.json under `bundle.resources`, so the Tauri
    // bundler ships it next to the spawned gateway binary in resource_dir.
    // The Gateway reads from `{exe_dir}/embedding_models.json` regardless
    // of how it got there (dev build script, package installer, or Tauri
    // bundler).
    //    These files are also bundled by Tauri resources, but in dev mode
    //    Gateway reads them from exe_dir (the bin/ staging directory).
    let lsp_config = workspace_root.join("assets").join("lsp_servers.json");
    if lsp_config.exists() {
        let dst = bin_dir.join("lsp_servers.json");
        let _ = std::fs::copy(&lsp_config, &dst);
    }

    let lsp_install_src = workspace_root.join("assets").join("lsp_install");
    let lsp_install_dst = bin_dir.join("lsp_install");
    if lsp_install_src.exists() {
        let _ = std::fs::create_dir_all(&lsp_install_dst);
        if let Ok(entries) = std::fs::read_dir(&lsp_install_src) {
            for entry in entries.flatten() {
                let src = entry.path();
                if src.is_file() {
                    let file_name = src.file_name().expect("path has no file name");
                    let dst = lsp_install_dst.join(file_name);
                    let _ = std::fs::copy(&src, &dst);
                }
            }
        }
    }

    // 6. Re-run if the profile changes (so switching between debug/release re-copies).
    println!("cargo:rerun-if-env-changed=PROFILE");

    // 7. Invoke Tauri build (processes tauri.conf.json).
    tauri_build::build()
}

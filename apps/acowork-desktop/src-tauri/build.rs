//! Build script for the ACowork desktop app.
//!
//! Before invoking `tauri_build::build()`, this script copies the core
//! workspace binaries (gateway, runtime, embed, pm, node, lsp-relay,
//! doc) and the ONNX runtime DLL into a `bin/` staging directory inside
//! `src-tauri/`. This allows `tauri.conf.json` to reference a fixed local
//! path instead of fragile `target/{profile}/` glob patterns that break
//! on a fresh clone.
//!
//! The `beforeDevCommand` / `beforeBuildCommand` in `tauri.conf.json` are
//! responsible for building the core workspace first, so the binaries
//! already exist in `target/{profile}/` by the time this script runs.
//!
//! **Dev builds never stage binaries in `bin/` (anti file-lock):**
//! `tauri_build::build()` reverse-copies every entry of `bundle.resources`
//! (`"bin/*": "./"`) into the dev resource dir (`target/{profile}/`, the
//! same directory the running Gateway / Node / ... processes were launched
//! from). When the core workspace is rebuilt, this script re-runs (see
//! step 6) and that reverse copy would try to overwrite the executables of
//! the live processes — Windows fails with `os error 32` (file in use).
//! The workspace binaries already exist in `target/{profile}/` (built by
//! `beforeDevCommand`), so staging them is redundant for `tauri dev`: dev
//! builds clear stale copies instead and keep only lock-free resources
//! (lsp_servers.json, lsp_install/) in `bin/`. Release packaging
//! (`tauri build`) keeps staging the full set so the installer still
//! bundles the workspace binaries.

use std::path::{Path, PathBuf};

/// Binaries to copy from the workspace target directory.
///
/// Must include every binary that the Gateway spawns as a sibling
/// (node, lsp-relay, doc) and every binary the Desktop bundles as a
/// `bin/*` resource — otherwise the reverse copy performed by
/// `tauri_build::build()` (resources → target/{profile}) resurrects
/// stale binaries from `src-tauri/bin/` over the fresh workspace build.
const BINARIES: &[&str] = &[
    "acowork-gateway",
    "acowork-runtime",
    "acowork-embed",
    "acowork-pm",
    "acowork-node",
    "acowork-lsp-relay",
    "acowork-doc",
];

/// Stage one workspace artifact into `bin/` for release packaging, or clear
/// a stale copy in dev builds (see module docs). Both behaviours live in one
/// place so a dev build can never overwrite a locked running binary by
/// accident. Returns `true` when a real file was copied.
fn stage_or_clear(is_dev: bool, src: &Path, dst: &Path) -> bool {
    if is_dev {
        let _ = std::fs::remove_file(dst);
        false
    } else if src.exists() {
        std::fs::copy(src, dst)
            .unwrap_or_else(|e| panic!("Failed to copy {}: {}", src.display(), e));
        true
    } else {
        false
    }
}

fn main() {
    // 1. Determine build profile and locate workspace target directory.
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    // manifest_dir = .../apps/acowork-desktop/src-tauri
    // Workspace root = .../ (3 levels up)
    let workspace_root = manifest_dir
        .parent() // apps/acowork-desktop
        .and_then(|p| p.parent()) // apps
        .and_then(|p| p.parent()) // workspace root
        .expect("Cannot determine workspace root from CARGO_MANIFEST_DIR");

    let target_dir = workspace_root.join("target").join(&profile);
    let bin_dir = manifest_dir.join("bin");

    // Dev builds must never stage workspace binaries into `bin/` — the
    // reverse copy of `tauri_build::build()` would collide with the live
    // Gateway / Node / ... executables (see module docs for details).
    let is_dev = tauri_build::is_dev();

    // 2. Create the staging directory.
    std::fs::create_dir_all(&bin_dir).expect("Failed to create bin/ staging directory");

    let exe_ext = if cfg!(windows) { ".exe" } else { "" };

    // 3. Stage each workspace binary (release) or clear stale copies (dev).
    //
    // Dev builds skip staging entirely: the binaries already exist in
    // target/{profile}/ and the reverse copy of `tauri_build::build()` must
    // not touch them while the dev-run processes hold file locks.
    for &name in BINARIES {
        let file_name = format!("{name}{exe_ext}");
        let src = target_dir.join(&file_name);
        if !is_dev && !src.exists() {
            println!(
                "cargo:warning=Binary not found: {} (run `cd core && cargo build -p {name}` first)",
                src.display()
            );
        } else {
            stage_or_clear(is_dev, &src, &bin_dir.join(&file_name));
        }
    }

    // 4. Stage the ONNX runtime shared library (release) or clear stale
    //    copies (dev) — same file-lock rationale as step 3.
    let ort_lib = if cfg!(windows) {
        "onnxruntime.dll"
    } else if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else {
        "libonnxruntime.so"
    };
    if stage_or_clear(is_dev, &target_dir.join(ort_lib), &bin_dir.join(ort_lib)) {
        println!("cargo:warning=Copied {ort_lib} to bin/");
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

    // 6. Re-run the build script when ANY file that build.rs copies changes.
    //
    // The `beforeBuildCommand` / `beforeDevCommand` in tauri.conf.json
    // compiles workspace binaries into `target/{profile}/` before `cargo
    // build` runs. Without these directives, Cargo caches the build script
    // when only copied files change (not the Tauri app source), and `bin/`
    // retains stale copies → the installer bundles old binaries.
    //
    // Binaries (updated by core:build commands).
    for &name in BINARIES {
        let path = target_dir.join(format!("{name}{exe_ext}"));
        println!("cargo:rerun-if-changed={}", path.display());
    }
    // ONNX runtime (platform-specific).
    if cfg!(windows) {
        println!("cargo:rerun-if-changed={}", target_dir.join("onnxruntime.dll").display());
    } else if cfg!(target_os = "macos") {
        println!("cargo:rerun-if-changed={}", target_dir.join("libonnxruntime.dylib").display());
    } else {
        println!("cargo:rerun-if-changed={}", target_dir.join("libonnxruntime.so").display());
    }
    // LSP config file.
    let lsp_config_path = workspace_root.join("assets").join("lsp_servers.json");
    println!("cargo:rerun-if-changed={}", lsp_config_path.display());
    // LSP install scripts (each file individually).
    let lsp_install_src = workspace_root.join("assets").join("lsp_install");
    if lsp_install_src.exists()
        && let Ok(entries) = std::fs::read_dir(&lsp_install_src)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }

    // 7. Invoke Tauri build (processes tauri.conf.json).
    tauri_build::build()
}

// ort_env.js — Smart ONNX Runtime auto-detection and environment setup
//
// Used by tauri.conf.json's beforeDevCommand so that `npm run tauri dev` works
// without any manual env-var setup on any platform.
//
// Search order:
//   1. .ort/onnxruntime-*/lib/{dll,dylib,so}  (from setup_ort.sh manual install)
//   2. ~/.cargo/registry/cache/...onnxruntime-*/lib/{dylib,so}
//      (auto-downloaded by `cargo build --features download-ort`)
//   3. None found → inject `--features download-ort[,coreml|cuda|directml]`
//      into any `cargo build -p acowork-embed` command automatically.
//
// Usage: node dev/ort_env.js <command...>

const { spawn } = require("child_process");
const fs = require("fs");
const path = require("path");
const os = require("os");

const workspaceRoot = path.resolve(__dirname, "..");
const ortBase = path.join(workspaceRoot, ".ort");

const isWin = process.platform === "win32";
const isMac = process.platform === "darwin";
const arch = process.arch;            // 'arm64' | 'x64'
const isAppleSilicon = isMac && arch === "arm64";

const libName = isWin
    ? "onnxruntime.dll"
    : isMac
        ? "libonnxruntime.dylib"
        : "libonnxruntime.so";

/**
 * Try to find a usable libonnxruntime file.
 * Returns { libDir, dylib, source } or null.
 */
function findOrt() {
    // ── Source 1: .ort/ directory ────────────────────────────────────────
    try {
        const entries = fs.readdirSync(ortBase, { withFileTypes: true });
        for (const e of entries) {
            if (!e.isDirectory() || !e.name.startsWith("onnxruntime-")) continue;
            const libDir = path.join(ortBase, e.name, "lib");
            const dylib = path.join(libDir, libName);
            if (fs.existsSync(dylib)) {
                return { libDir, dylib, source: e.name };
            }
        }
    } catch (_) {
        // .ort/ not found or unreadable
    }

    // ── Source 2: Cargo registry cache (from `download-ort` feature) ────
    const cargoCache = path.join(os.homedir(), ".cargo", "registry", "cache");
    if (fs.existsSync(cargoCache)) {
        // Walk one level deep into each cache entry
        try {
            for (const cacheEntry of fs.readdirSync(cargoCache)) {
                const full = path.join(cargoCache, cacheEntry);
                if (!fs.statSync(full).isDirectory()) continue;
                try {
                    for (const sub of fs.readdirSync(full)) {
                        if (!sub.startsWith("onnxruntime-")) continue;
                        const libDir = path.join(full, sub, "lib");
                        const dylib = path.join(libDir, libName);
                        if (fs.existsSync(dylib)) {
                            return { libDir, dylib, source: `cargo-cache:${cacheEntry}/${sub}` };
                        }
                    }
                } catch (_) {}
            }
        } catch (_) {}
    }

    // ── Source 3: Cargo registry src (unpacked) ─────────────────────────
    const cargoSrc = path.join(os.homedir(), ".cargo", "registry", "src");
    if (fs.existsSync(cargoSrc)) {
        try {
            for (const cacheEntry of fs.readdirSync(cargoSrc)) {
                const full = path.join(cargoSrc, cacheEntry);
                if (!fs.statSync(full).isDirectory()) continue;
                try {
                    for (const sub of fs.readdirSync(full)) {
                        if (!sub.startsWith("onnxruntime-")) continue;
                        const libDir = path.join(full, sub, "lib");
                        const dylib = path.join(libDir, libName);
                        if (fs.existsSync(dylib)) {
                            return { libDir, dylib, source: `cargo-src:${cacheEntry}/${sub}` };
                        }
                    }
                } catch (_) {}
            }
        } catch (_) {}
    }

    return null;
}

/**
 * Decide which `download-ort[,xxx]` features to inject on `cargo build -p acowork-embed`.
 */
function pickEmbedFeatures() {
    // Apple Silicon → CoreML
    if (isAppleSilicon) return "download-ort,coreml";
    // Windows → DirectML
    if (isWin) return "download-ort,directml";
    // Linux / Intel Mac → CPU only
    return "download-ort";
}

const ort = findOrt();
const env = { ...process.env };

if (ort) {
    env.ORT_LIB_LOCATION = ort.libDir;
    env.ORT_DYLIB_PATH = ort.dylib;
    env.ORT_PREFER_DYNAMIC_LINK = "1";
    console.log(`[ort_env] ✓ ONNX Runtime found: ${ort.source}`);
    console.log(`[ort_env]   ORT_LIB_LOCATION=${ort.libDir}`);
} else {
    console.log(`[ort_env] ⚠ ONNX Runtime not found in .ort/ or cargo cache`);
    console.log(`[ort_env]   Will auto-inject download-ort feature into cargo build`);
}

let args = process.argv.slice(2);
if (args.length === 0) {
    console.error("Usage: node dev/ort_env.js <command...>");
    process.exit(1);
}

// ── Auto-inject --features download-ort[,xxx] for acowork-embed ────────────
// If ORT is not configured AND the user is running `cargo build ... -p acowork-embed`,
// inject the right features so the build can fetch ORT automatically.
if (!ort) {
    const isCargoBuild = args[0] === "cargo" && (args[1] === "build" || args[1] === "check" || args[1] === "test");

    // Scan all `-p <crate>` pairs to find any acowork-embed target.
    // Cargo allows multiple -p flags (e.g. -p gateway -p runtime -p embed),
    // so we must iterate, not just look at args.indexOf("-p") + 1.
    function targetsEmbedCrate(argv) {
        for (let i = 0; i < argv.length; i++) {
            if (argv[i] === "-p" || argv[i] === "--package") {
                if (argv[i + 1] === "acowork-embed") return true;
            }
        }
        return false;
    }
    const targetsEmbed = targetsEmbedCrate(args);

    // Find any existing --features flag and its value(s).
    // --features can be repeated; we treat any flag whose value mentions download-ort as "already covered".
    const hasDownload = args.some((a, i) => a === "--features" && (args[i + 1] || "").includes("download-ort"));
    const hasAnyFeatures = args.some((a, i) => a === "--features");

    if (isCargoBuild && targetsEmbed) {
        if (!hasDownload) {
            const features = pickEmbedFeatures();
            // Find where the cargo command ends (next shell operator: && || ;).
            // --features must go BEFORE the operator, otherwise it ends up
            // attached to whatever comes after (e.g. `npm run dev`).
            const SHELL_OPS = new Set(["&&", "||", ";", "|", ">", ">>", "<"]);
            let insertAt = args.length;
            for (let i = 1; i < args.length; i++) {
                if (SHELL_OPS.has(args[i])) { insertAt = i; break; }
            }

            // If the user already provided --features in the cargo segment, append to it.
            let appended = false;
            for (let i = 1; i < insertAt; i++) {
                if (args[i] === "--features") {
                    args[i + 1] = `${args[i + 1]},${features}`;
                    appended = true;
                    console.log(`[ort_env] → appending ${features} to existing --features at arg #${i}`);
                    break;
                }
            }
            if (!appended) {
                args.splice(insertAt, 0, "--features", features);
                console.log(`[ort_env] → injecting --features ${features} before ${args[insertAt + 2] || "<end>"}`);
            }
        } else {
            console.log(`[ort_env] → download-ort already present in --features, skipping`);
        }
    } else if (isCargoBuild) {
        console.log(`[ort_env] → command does not target acowork-embed, skipping feature injection`);
    }
}

// On Windows, npm runs scripts through cmd.exe. Spawn a shell so that
// `&&` chaining in the caller's command line continues to work.
const shell = isWin ? "cmd.exe" : "/bin/sh";
const shellFlag = isWin ? "/c" : "-c";
const cmd = args.join(" ");

console.log(`[ort_env] $ ${cmd}\n`);

const child = spawn(shell, [shellFlag, cmd], { env, stdio: "inherit" });
child.on("exit", (code, signal) => {
    if (signal) {
        process.kill(process.pid, signal);
    }
    process.exit(code ?? 1);
});

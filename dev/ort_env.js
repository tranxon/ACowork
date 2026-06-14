// ort_env.js — Auto-detect local ONNX Runtime and set env vars before running a command.
//
// Used by tauri.conf.json's beforeDevCommand so that `npm run tauri dev` works
// without any manual env-var setup on any platform.
//
// Usage: node dev/ort_env.js <command...>
// Example: node dev/ort_env.js cargo build --manifest-path core/Cargo.toml -p rollball-embed

const { spawn } = require("child_process");
const fs = require("fs");
const path = require("path");

const workspaceRoot = path.resolve(__dirname, "..");
const ortBase = path.join(workspaceRoot, ".ort");

function findOrt() {
    try {
        const entries = fs.readdirSync(ortBase, { withFileTypes: true });
        const isWin = process.platform === "win32";
        const libName = isWin
            ? "onnxruntime.dll"
            : process.platform === "darwin"
                ? "libonnxruntime.dylib"
                : "libonnxruntime.so";
        for (const e of entries) {
            if (!e.isDirectory() || !e.name.startsWith("onnxruntime-")) continue;
            const libDir = path.join(ortBase, e.name, "lib");
            const dylib = path.join(libDir, libName);
            if (fs.existsSync(dylib)) {
                return { libDir, dylib };
            }
        }
    } catch (_) {
        // .ort/ not found
    }
    return null;
}

const ort = findOrt();
const env = { ...process.env };
if (ort) {
    env.ORT_LIB_LOCATION = ort.libDir;
    env.ORT_DYLIB_PATH = ort.dylib;
    env.ORT_PREFER_DYNAMIC_LINK = "1";
    console.log(`ORT auto-detected: ${ort.libDir}`);
}

const args = process.argv.slice(2);
if (args.length === 0) {
    console.error("Usage: node dev/ort_env.js <command...>");
    process.exit(1);
}

// On Windows, npm runs scripts through cmd.exe. Spawn a shell so that
// `&&` chaining in the caller's command line continues to work.
const shell = process.platform === "win32" ? "cmd.exe" : "/bin/sh";
const shellFlag = process.platform === "win32" ? "/c" : "-c";
const cmd = args.join(" ");

const child = spawn(shell, [shellFlag, cmd], { env, stdio: "inherit" });
child.on("exit", (code, signal) => {
    if (signal) {
        process.kill(process.pid, signal);
    }
    process.exit(code ?? 1);
});

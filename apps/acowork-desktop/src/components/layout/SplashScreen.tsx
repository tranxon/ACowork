import { useState, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { getGatewayUrl } from "../../lib/config";
// eslint-disable-next-line @typescript-eslint/no-require-imports
import pkg from "../../../package.json";

const POLL_INTERVAL = 500;
const MIN_SPLASH_MS = 1500;
const MAX_WAIT_MS = 20_000;
const SYSTEM_AGENT_ID = "com.acowork.system";

/**
 * Push the persisted settings into Rust and (for local mode) spawn the
 * Gateway child process. Returns the URL the frontend should poll.
 *
 * This replaces the previous architecture where Rust unconditionally
 * spawned the local Gateway in its setup hook, hardcoding the URL and
 * ignoring the frontend's "remote gateway" setting.
 */
async function bootGateway(): Promise<void> {
    const settings = useSettingsStore.getState();
    const mode = settings.gatewayMode;
    const url = settings.gatewayUrl;

    // 1) Sync config into Rust. Rust will:
    //    - local: force base_url = defaults::GATEWAY_HTTP_URL (ignores `url`)
    //    - remote: store the user-configured URL; if a local process is
    //      already running it will be stopped to free the port.
    await invoke("set_gateway_config", {
        config: { mode, url },
    });

    // 2) Boot the gateway itself
    if (mode === "local") {
        // Spawns child Gateway + waits up to 10s for /health
        await invoke("init_local_gateway");
        // Sync the frontend store with the Rust-side process handle.
        // `init_local_gateway` bypasses `startLocalGateway()` (the store
        // action), so without this call `localState` would stay "idle"
        // even though the backend has a running child process. This
        // causes the Settings page to show a spurious "Start Gateway"
        // button next to the green "Running" indicator.
        await useGatewayStore.getState().checkLocalStatus();
    }
    // Remote mode: the Gateway is presumed already running on `url`.

    // 3) Ensure System Agent is installed on whichever Gateway we ended
    //    up with. Rust uses its internal base_url (already configured).
    try {
        await invoke("ensure_system_agent");
    } catch (err) {
        // Non-fatal: user can install agents manually
        console.warn("ensure_system_agent failed:", err);
    }
}

interface SplashScreenProps {
    onReady: () => void;
}

function LoadingDots({ className = "" }: { className?: string }) {
    const [dots, setDots] = useState(0);
    useEffect(() => {
        const id = setInterval(() => setDots((d) => (d + 1) % 4), 400);
        return () => clearInterval(id);
    }, []);
    return (
        <span className={className}>
            {".".repeat(dots)}
            <span className="opacity-0">{".".repeat(3 - dots)}</span>
        </span>
    );
}

export function SplashScreen({ onReady }: SplashScreenProps) {
    const checkHealth = useGatewayStore((s) => s.checkHealth);
    const startLocalGateway = useGatewayStore((s) => s.startLocalGateway);
    const gatewayMode = useSettingsStore((s) => s.gatewayMode);
    const [statusText, setStatusText] = useState("Starting Gateway...");
    const [timedOut, setTimedOut] = useState(false);
    const [retrying, setRetrying] = useState(false);
    const [fadeIn, setFadeIn] = useState(false);
    const mountedRef = useRef(true);
    const startTimeRef = useRef(Date.now());

    useEffect(() => {
        requestAnimationFrame(() => setFadeIn(true));
    }, []);

    /** Check if System Agent is ready via GET /api/agents */
    const checkAgentReady = async (): Promise<boolean> => {
        if (!mountedRef.current) return false;
        try {
            const resp = await fetch(`${getGatewayUrl()}/api/agents`);
            if (!resp.ok) return false;
            const agents = (await resp.json()) as Array<{ agent_id: string; ready: boolean; running: boolean }>;
            const sys = agents?.find((a) => a.agent_id === SYSTEM_AGENT_ID);
            return !!(sys && sys.ready);
        } catch {
            return false;
        }
    };

    const doCheck = async (): Promise<boolean> => {
        if (!mountedRef.current) return false;
        await checkHealth();
        if (useGatewayStore.getState().status === "connected") {
            return true;
        }
        return false;
    };

    const finish = () => {
        if (!mountedRef.current) return;
        const elapsed = Date.now() - startTimeRef.current;
        const remaining = Math.max(0, MIN_SPLASH_MS - elapsed);
        setStatusText("Ready");
        setTimeout(() => {
            if (mountedRef.current) onReady();
        }, remaining);
    };

    useEffect(() => {
        startTimeRef.current = Date.now();
        mountedRef.current = true;

        let pollTimer: ReturnType<typeof setInterval>;
        let maxTimer: ReturnType<typeof setTimeout>;

        const pollGateway = (onDone: () => void) => {
            pollTimer = setInterval(async () => {
                const done = await doCheck();
                if (done) {
                    clearInterval(pollTimer);
                    onDone();
                }
            }, POLL_INTERVAL);
        };

        const pollAgentReady = () => {
            setStatusText("Waiting for Agent Runtime...");
            // Try immediately first
            checkAgentReady().then((ready) => {
                if (ready) { finish(); return; }
                // Then poll
                pollTimer = setInterval(async () => {
                    if (!mountedRef.current) { clearInterval(pollTimer); return; }
                    const ready = await checkAgentReady();
                    if (ready) {
                        clearInterval(pollTimer);
                        finish();
                    }
                }, POLL_INTERVAL);
            });
        };

        const init = async () => {
            if (gatewayMode === "local") {
                setStatusText("Starting local Gateway...");
            } else {
                setStatusText("Connecting to Gateway...");
            }

            // Push persisted settings into Rust and (for local mode)
            // spawn the child Gateway. The previous architecture did
            // this in Rust's setup hook, which ignored the user's
            // remote-gateway setting.
            try {
                await bootGateway();
            } catch (err) {
                console.error("bootGateway failed:", err);
                // Fall through to health polling — Gateway may still be
                // reachable from a previous run.
            }

            const gDone = await doCheck();
            if (gDone) {
                pollAgentReady();
                return;
            }
            pollGateway(() => pollAgentReady());
        };

        maxTimer = setTimeout(() => {
            if (mountedRef.current) {
                clearInterval(pollTimer);
                setTimedOut(true);
            }
        }, MAX_WAIT_MS);

        init();

        return () => {
            mountedRef.current = false;
            clearInterval(pollTimer);
            clearTimeout(maxTimer);
        };
    }, []); // eslint-disable-line react-hooks/exhaustive-deps

    const handleRetry = async () => {
        setRetrying(true);
        setTimedOut(false);
        startTimeRef.current = Date.now();
        if (gatewayMode === "local") {
            setStatusText("Retrying local Gateway...");
            await startLocalGateway();
        } else {
            setStatusText("Retrying connection...");
        }
        const gDone = await doCheck();
        if (gDone) {
            // Poll for agent readiness
            const ready = await checkAgentReady();
            if (ready) {
                finish();
            } else {
                // Quick poll a few times
                for (let i = 0; i < 20; i++) {
                    if (!mountedRef.current) break;
                    await new Promise((r) => setTimeout(r, 500));
                    if (await checkAgentReady()) { finish(); break; }
                }
                if (mountedRef.current && !useGatewayStore.getState().status) setTimedOut(true);
            }
        }
        setRetrying(false);
    };

    return (
        <div className="relative flex h-screen w-screen flex-col items-center justify-center overflow-hidden bg-zinc-50 dark:bg-zinc-900">
            <div
                className={`relative z-10 flex flex-col items-center gap-10 transition-all duration-700 ${fadeIn ? "translate-y-0 opacity-100" : "translate-y-4 opacity-0"
                    }`}
            >
                {/* Title — Acowork brand text (no icon) */}
                <div className="flex flex-col items-center gap-5">
                    <h1 className="text-[88px] font-bold leading-none tracking-[-0.03em]">
                        {/* Layered A: three solid A glyphs stacked, each shifted right by 3% of the A's own width. The invisible A establishes the wrapper width so the percentage offsets are relative to the glyph. */}
                        <span className="relative inline-block">
                            <span className="invisible" aria-hidden="true">A</span>
                            <span className="absolute left-[6%] top-0" style={{ color: "#4285F4" }} aria-hidden="true">A</span>
                            <span className="absolute left-[3%] top-0" style={{ color: "#34A853" }} aria-hidden="true">A</span>
                            <span className="absolute left-0 top-0" style={{ color: "#FBBC05" }} aria-hidden="true">A</span>
                        </span>
                        <span style={{ color: "#34A853" }}>co</span>
                        <span style={{ color: "#4285F4" }}>work</span>
                    </h1>
                    <p className="text-sm font-medium uppercase tracking-[0.25em] text-zinc-400 dark:text-zinc-500">
                        Collaborate with your AI colleagues
                    </p>
                </div>

                {/* Status area */}
                <div className="flex min-h-[60px] flex-col items-center gap-3">
                    {timedOut ? (
                        <>
                            <div className="flex items-center gap-2">
                                <div className="h-2 w-2 animate-pulse rounded-full bg-amber-500" />
                                <p className="text-sm text-zinc-600 dark:text-zinc-400">
                                    Gateway did not respond within {MAX_WAIT_MS / 1000}s
                                </p>
                            </div>
                            <p className="text-xs text-zinc-400 dark:text-zinc-500">
                                Make sure the Gateway is running on port 19876
                            </p>
                            <button
                                onClick={handleRetry}
                                disabled={retrying}
                                className="mt-2 rounded-md bg-zinc-200 px-5 py-2 text-sm font-medium text-zinc-700 transition-colors hover:bg-zinc-300 disabled:opacity-40 dark:bg-zinc-700 dark:text-zinc-200 dark:hover:bg-zinc-600"
                            >
                                {retrying ? "Retrying..." : "Retry Connection"}
                            </button>
                        </>
                    ) : (
                        <>
                            {/* Progress bar */}
                            <div className="h-0.5 w-48 overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-700">
                                <div
                                    className="h-full animate-[pulse_1.5s_ease-in-out_infinite] rounded-full"
                                    style={{
                                        background:
                                            "linear-gradient(90deg, #10b981, #06b6d4, #34d399, #10b981)",
                                        backgroundSize: "200% 100%",
                                        animation:
                                            "shimmer 2s ease-in-out infinite, pulse 1.5s ease-in-out infinite",
                                    }}
                                />
                            </div>
                            <p className="text-sm text-zinc-500 dark:text-zinc-400">
                                {statusText}
                                <LoadingDots className="inline-block w-5 text-left text-zinc-400 dark:text-zinc-500" />
                            </p>
                        </>
                    )}
                </div>
            </div>

            {/* Version in bottom corner */}
            <div
                className={`absolute bottom-6 transition-all duration-700 delay-300 ${fadeIn ? "translate-y-0 opacity-100" : "translate-y-2 opacity-0"
                    }`}
            >
                <span className="text-[11px] text-zinc-400 dark:text-zinc-600">AgentCowork v{pkg.version}</span>
            </div>

            {/* Keyframe styles injected once */}
            <style>{`
                @keyframes shimmer {
                    0% { background-position: 200% 0; }
                    100% { background-position: -200% 0; }
                }
            `}</style>
        </div>
    );
}

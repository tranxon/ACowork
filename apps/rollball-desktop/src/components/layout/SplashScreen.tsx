import { useState, useEffect, useRef } from "react";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { getGatewayUrl } from "../../lib/config";
// eslint-disable-next-line @typescript-eslint/no-require-imports
import pkg from "../../../package.json";

const POLL_INTERVAL = 500;
const MIN_SPLASH_MS = 1500;
const MAX_WAIT_MS = 20_000;
const SYSTEM_AGENT_ID = "com.rollball.system";

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
                await startLocalGateway();
            } else {
                setStatusText("Connecting to Gateway...");
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
                className={`relative z-10 flex flex-col items-center gap-8 transition-all duration-700 ${fadeIn ? "translate-y-0 opacity-100" : "translate-y-4 opacity-0"
                    }`}
            >
                {/* Logo mark */}
                <div className="relative">
                    {/* Outer ring animation */}
                    <div className="absolute inset-0 animate-[spin_8s_linear_infinite] opacity-25">
                        <svg className="h-20 w-20" viewBox="0 0 100 100" fill="none">
                            <circle
                                cx="50"
                                cy="50"
                                r="48"
                                stroke="url(#ringGrad)"
                                strokeWidth="1.5"
                                strokeDasharray="80 220"
                                strokeLinecap="round"
                            />
                            <defs>
                                <linearGradient id="ringGrad" x1="0" y1="0" x2="100" y2="100">
                                    <stop offset="0%" stopColor="#10b981" />
                                    <stop offset="100%" stopColor="#06b6d4" />
                                </linearGradient>
                            </defs>
                        </svg>
                    </div>
                    {/* Inner icon */}
                    <div className="relative flex h-20 w-20 items-center justify-center rounded-2xl bg-gradient-to-br from-emerald-500 to-cyan-400">
                        <svg
                            className="h-9 w-9 text-white"
                            viewBox="0 0 24 24"
                            fill="none"
                            stroke="currentColor"
                            strokeWidth="2"
                            strokeLinecap="round"
                            strokeLinejoin="round"
                        >
                            <circle cx="12" cy="12" r="10" />
                            <path d="M8 8h5a3 3 0 0 1 0 6H8V8Z" />
                            <path d="M8 14h4l3 3" />
                        </svg>
                    </div>
                </div>

                {/* Title */}
                <div className="flex flex-col items-center gap-1.5">
                    <h1 className="text-[32px] font-bold tracking-tight text-zinc-800 dark:text-zinc-100">
                        RollBall.AI
                    </h1>
                    <p className="text-xs font-medium uppercase tracking-[0.2em] text-zinc-400 dark:text-zinc-500">
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
                                className="mt-2 rounded-lg bg-zinc-200 px-5 py-2 text-sm font-medium text-zinc-700 transition-colors hover:bg-zinc-300 disabled:opacity-40 dark:bg-zinc-700 dark:text-zinc-200 dark:hover:bg-zinc-600"
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
                <span className="text-[11px] text-zinc-400 dark:text-zinc-600">RollBall v{pkg.version}</span>
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

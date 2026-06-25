import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { useAgentStore } from "../../stores/agentStore";
import { cn } from "../../lib/utils";
import { needsApiKey, keyPlaceholder } from "../../lib/providers";
import { fetchProviderModels, fetchProviders, createUser } from "../../lib/gateway-api";
import { DEFAULT_GATEWAY_URL } from "../../lib/config";
import type { GatewayMode, ModelInfo } from "../../lib/types";
import { RadioGroup } from "../common/RadioGroup";
import { StyledInput } from "../common/StyledInput";
import brandMark from "../../../../../assets/brand-mark.svg";

const TOTAL_STEPS = 5;

const RECOMMENDED_AGENTS = [
  {
    resourceName: "software-architect-agent",
    name: "Architect",
    role: "Software Architect",
    description: "System design, architecture review, technical planning, and risk assessment",
  },
  {
    resourceName: "senior-engineer-agent",
    name: "SSE",
    role: "Senior Software Engineer",
    description: "Code review, architecture design, debugging, refactoring, testing, and documentation",
  },
  {
    resourceName: "quality-assurance-agent",
    name: "QA",
    role: "Quality Assurance Manager",
    description: "Quality strategy, test planning, defect management, and release acceptance",
  },
  {
    resourceName: "project-manager-agent",
    name: "PM",
    role: "Project Manager",
    description: "Requirements analysis, task decomposition, progress tracking, and risk management",
  },
  {
    resourceName: "product-manager-agent",
    name: "Product",
    role: "Product Manager",
    description: "Product strategy, user research, PRD writing, roadmap, and launch planning",
  },
  {
    resourceName: "document-manager-agent",
    name: "Docs",
    role: "Document Manager",
    description: "Document collection, organization, writing, conversion, and knowledge base maintenance",
  },
];

interface OnboardingState {
  completed: boolean;
  currentStep: number;
  // Step 4: identity
  name: string;
  language: string;
  timezone: string;
  city: string;
  occupation: string;
}

export function OnboardingFlow({ onComplete }: { onComplete?: () => void }) {
  const [state, setState] = useState<OnboardingState>({
    completed: false,
    currentStep: 1,
    name: "",
    language: "zh-CN",
    timezone: "Asia/Shanghai",
    city: "",
    occupation: "",
  });

  // Check if onboarding was already completed
  useEffect(() => {
    const saved = localStorage.getItem("acowork_onboarding");
    if (saved === "completed") {
      setState((prev) => ({ ...prev, completed: true }));
    }
  }, []);

  const completeOnboarding = useCallback(() => {
    // Persist user identity to Gateway if name was provided in Step 4.
    // Fire-and-forget — don't block onboarding completion on API result.
    if (state.name.trim()) {
      createUser({
        display_name: state.name.trim(),
        language: state.language,
        timezone: state.timezone,
        city: state.city.trim() || undefined,
        occupation: state.occupation.trim() || undefined,
      }).catch((err) => {
        console.warn("Failed to create user profile during onboarding:", err);
      });
      // Sync the display name to the local profile store so the avatar
      // picker in ProfileTab immediately shows the name just entered.
      useUserProfileStore.getState().setProfile({ displayName: state.name.trim() });
    }
    // Assign a random builtin avatar if the user doesn't have one yet,
    // so the freshly-onboarded user shows a real icon (not the legacy
    // letter/gradient fallback) from the very first session.
    useUserProfileStore.getState().assignRandomAvatarIfMissing();
    localStorage.setItem("acowork_onboarding", "completed");
    setState((prev) => ({ ...prev, completed: true }));
    onComplete?.();
  }, [onComplete, state.name, state.language, state.timezone, state.city, state.occupation]);

  const nextStep = () => setState((prev) => ({ ...prev, currentStep: Math.min(prev.currentStep + 1, TOTAL_STEPS) }));
  const prevStep = () => setState((prev) => ({ ...prev, currentStep: Math.max(prev.currentStep - 1, 1) }));

  if (state.completed) return null;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-white dark:bg-zinc-900">
      <div className="w-full max-w-md px-8">
        {/* Progress bar */}
        <div className="mb-8">
          <div className="flex items-center gap-1">
            {Array.from({ length: TOTAL_STEPS }, (_, i) => (
              <div
                key={i}
                className={cn(
                  "h-1 flex-1 rounded-full transition-colors",
                  i < state.currentStep ? "bg-zinc-800 dark:bg-zinc-200" : "bg-zinc-200 dark:bg-zinc-700",
                )}
              />
            ))}
          </div>
          <p className="mt-2 text-xs text-zinc-400">Step {state.currentStep} of {TOTAL_STEPS}</p>
        </div>

        {/* Step content */}
        {state.currentStep === 1 && <WelcomeStep onNext={nextStep} onSkip={completeOnboarding} />}
        {state.currentStep === 2 && <GatewayStep onNext={nextStep} onPrev={prevStep} />}
        {state.currentStep === 3 && <ApiKeyStep onNext={nextStep} onPrev={prevStep} />}
        {state.currentStep === 4 && (
          <IdentityStep
            name={state.name}
            language={state.language}
            timezone={state.timezone}
            city={state.city}
            occupation={state.occupation}
            onUpdate={(updates) => setState((prev) => ({ ...prev, ...updates }))}
            onNext={nextStep}
            onPrev={prevStep}
          />
        )}
        {state.currentStep === 5 && <InstallAgentStep onComplete={completeOnboarding} onPrev={prevStep} />}
      </div>
    </div>
  );
}

/** Step 1: Welcome */
function WelcomeStep({ onNext, onSkip }: { onNext: () => void; onSkip: () => void }) {
  return (
    <div className="text-center">
      <div className="text-4xl">🎉🎉🎉</div>
      <h1 className="mt-4 text-2xl font-bold flex items-center justify-center gap-2"><span>Welcome to</span><img src={brandMark} alt="Acowork" className="h-10" /></h1>
      <p className="mt-2 text-sm text-zinc-500">Let's quickly set up your Agent environment</p>
      <div className="mt-8 space-y-3">
        <button
          onClick={onNext}
          className="w-full rounded btn-solid py-2.5 text-sm font-medium"
        >
          Start Setup
        </button>
        <button
          onClick={onSkip}
          className="w-full py-2 text-xs text-zinc-400 hover:text-zinc-600 dark:hover:text-zinc-300"
        >
          Already configured? Skip setup →
        </button>
      </div>
    </div>
  );
}

/** Step 2: Gateway Connection — mode selection + connect */
function GatewayStep({ onNext, onPrev }: { onNext: () => void; onPrev: () => void }) {
  const { status, localState, checkHealth, startLocalGateway } = useGatewayStore();
  const gatewayMode = useSettingsStore((s) => s.gatewayMode);
  const setGatewayMode = useSettingsStore((s) => s.setGatewayMode);
  const gatewayUrl = useSettingsStore((s) => s.gatewayUrl);
  const setGatewayUrl = useSettingsStore((s) => s.setGatewayUrl);
  const [urlDraft, setUrlDraft] = useState(gatewayUrl);
  const [checking, setChecking] = useState(false);
  const [starting, setStarting] = useState(false);
  const [startError, setStartError] = useState<string | null>(null);

  // Sync urlDraft when gatewayUrl changes externally
  useEffect(() => { setUrlDraft(gatewayUrl); }, [gatewayUrl]);

  // Preload provider list as soon as Gateway is connected.
  useEffect(() => {
    if (status === "connected") {
      fetchProviders().catch(() => { });
    }
  }, [status]);

  // Auto-check health when mode changes or on mount (both local and remote)
  useEffect(() => {
    checkHealth();
  }, [gatewayMode, checkHealth]);

  const handleModeChange = (mode: GatewayMode) => {
    setGatewayMode(mode);
    setStartError(null);
  };

  const handleStartLocal = async () => {
    setStarting(true);
    setStartError(null);
    try {
      await startLocalGateway();
      // After successful start, health is already checked
    } catch {
      setStartError("Failed to start local Gateway");
    } finally {
      setStarting(false);
    }
  };

  const handleTestRemote = async () => {
    // Save URL first
    setGatewayUrl(urlDraft.trim());
    setChecking(true);
    await checkHealth();
    setChecking(false);
  };

  const canProceed = gatewayMode === "local"
    ? status === "connected"
    : status === "connected";

  const localConnected = gatewayMode === "local" && status === "connected";
  const localError = gatewayMode === "local" && (startError || localState === "error");

  return (
    <div>
      <h2 className="text-lg font-semibold">Connect to Gateway</h2>
      <p className="mt-1 text-sm text-zinc-500">Choose how you want to connect to the Gateway</p>

      <div className="mt-6 space-y-4">
        {/* Mode selection */}
        <div className="rounded-md border border-zinc-200 p-4 dark:border-zinc-700">
          <label className="mb-2 block text-xs text-zinc-500">Gateway Mode</label>
          <RadioGroup
            name="gatewayMode"
            value={gatewayMode}
            options={[
              { label: <span className="font-medium">Local (recommended)</span>, value: "local" as GatewayMode },
              { label: "Remote", value: "remote" as GatewayMode },
            ]}
            onChange={handleModeChange}
          />
          {gatewayMode === "local" && (
            <p className="mt-1 text-xs text-zinc-400">
              Gateway starts automatically with the Desktop App
            </p>
          )}
          {gatewayMode === "remote" && (
            <p className="mt-1 text-xs text-zinc-400">
              Connect to a Gateway running on another machine
            </p>
          )}
        </div>

        {/* Local mode: auto-start status */}
        {gatewayMode === "local" && (
          <div className="rounded-md border border-zinc-200 p-4 dark:border-zinc-700">
            <div className="flex items-center gap-2 text-sm">
              <span className="text-zinc-500">Status:</span>
              {starting ? (
                <span className="text-zinc-400">Starting Gateway...</span>
              ) : localConnected ? (
                <>
                  <span className="h-2 w-2 rounded-full bg-green-500" />
                  <span className="text-green-600 dark:text-green-400">Gateway Connected</span>
                </>
              ) : (
                <>
                  <span className="h-2 w-2 rounded-full bg-amber-500" />
                  <span className="text-amber-600 dark:text-amber-400">
                    {localError ? "Failed to start" : "Not started"}
                  </span>
                </>
              )}
            </div>
            {localError && (
              <p className="mt-1 text-xs text-red-500">{startError ?? "Could not start local Gateway"}</p>
            )}
            {!localConnected && !starting && (
              <button
                onClick={handleStartLocal}
                className="mt-3 rounded-md px-3 py-1.5 text-xs font-medium text-white hover:opacity-90"
                style={{ backgroundColor: "var(--color-accent)" }}
              >
                Start Local Gateway
              </button>
            )}
          </div>
        )}

        {/* Remote mode: URL config + test */}
        {gatewayMode === "remote" && (
          <div className="rounded-md border border-zinc-200 p-4 dark:border-zinc-700">
            <label className="mb-1 block text-xs text-zinc-500">Gateway URL</label>
            <div className="flex gap-2">
              <input
                type="text"
                value={urlDraft}
                onChange={(e) => setUrlDraft(e.target.value)}
                placeholder={DEFAULT_GATEWAY_URL}
                className="flex-1 rounded-md border border-zinc-200 px-3 py-2 text-sm dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
              />
              {urlDraft !== gatewayUrl && (
                <button
                  onClick={() => { setGatewayUrl(urlDraft.trim()); checkHealth(); }}
                  className="rounded-md px-3 py-2 text-xs font-medium text-white hover:opacity-90"
                  style={{ backgroundColor: "var(--color-accent)" }}
                >
                  Apply
                </button>
              )}
            </div>

            <div className="mt-3 flex items-center gap-2 text-sm">
              <span className="text-zinc-500">Status:</span>
              {checking ? (
                <span className="text-zinc-400">Checking...</span>
              ) : status === "connected" ? (
                <>
                  <span className="h-2 w-2 rounded-full bg-green-500" />
                  <span className="text-green-600 dark:text-green-400">Connected</span>
                </>
              ) : (
                <>
                  <span className="h-2 w-2 rounded-full bg-red-500" />
                  <span className="text-red-600 dark:text-red-400">Cannot connect</span>
                </>
              )}
            </div>

            {status !== "connected" && (
              <button
                onClick={handleTestRemote}
                disabled={checking || !urlDraft.trim()}
                className="mt-3 rounded btn-solid px-3 py-1.5 text-xs font-medium disabled:opacity-50"
              >
                {checking ? "Testing..." : "Test Connection"}
              </button>
            )}
          </div>
        )}
      </div>

      <div className="mt-8 flex justify-between">
        <button onClick={onPrev} className="rounded-md px-4 py-2 text-xs text-zinc-500 hover:text-zinc-700 dark:hover:text-zinc-300">
          Back
        </button>
        <button
          onClick={onNext}
          disabled={!canProceed || starting}
          className="rounded btn-solid px-4 py-2 text-xs font-medium disabled:opacity-50"
        >
          {starting ? "Starting..." : "Next"}
        </button>
      </div>
    </div>
  );
}

/** Step 3: API Key */
function ApiKeyStep({ onNext, onPrev }: { onNext: () => void; onPrev: () => void }) {
  const [provider, setProvider] = useState("openai");
  const [dynamicProviders, setDynamicProviders] = useState<Array<{ id: string; name: string; api?: string }>>([]);
  const [apiKey, setApiKey] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [selectedModels, setSelectedModels] = useState<string[]>([]);
  const [availableModels, setAvailableModels] = useState<ModelInfo[]>([]);
  const [modelsLoading, setModelsLoading] = useState(false);
  const [modelSearchTerm, setModelSearchTerm] = useState("");
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);

  // Fetch dynamic providers from Gateway API
  useEffect(() => {
    const loadProviders = async () => {
      try {
        const providers = await fetchProviders();
        setDynamicProviders(providers);
      } catch {
        setDynamicProviders([]);
      }
    };
    loadProviders();
  }, []);

  const loadModels = useCallback(async (providerId: string) => {
    setModelsLoading(true);
    try {
      const data = await fetchProviderModels(providerId);
      setAvailableModels(data.models ?? []);
    } catch {
      setAvailableModels([]);
    }
    setModelsLoading(false);
  }, []);

  // Update base URL when provider changes
  const handleProviderChange = (id: string) => {
    setProvider(id);
    setSaved(false);
    setSelectedModels([]);
    setModelSearchTerm("");
    const dynamicProvider = dynamicProviders.find((p) => p.id === id);
    setBaseUrl(dynamicProvider?.api ?? "");
    loadModels(id);
  };

  // Load initial models
  useEffect(() => {
    loadModels(provider);
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const handleSave = async () => {
    setSaving(true);
    try {
      await invoke("add_key", {
        provider,
        key: apiKey,
        baseUrl: baseUrl || undefined,
        defaultModel: selectedModels.length > 0 ? selectedModels[0] : undefined,
      });
      setSaved(true);
    } catch {
      // Continue anyway
    } finally {
      setSaving(false);
    }
  };

  const toggleModel = (model: string) => {
    if (selectedModels.includes(model)) {
      setSelectedModels(selectedModels.filter((m) => m !== model));
    } else {
      setSelectedModels([...selectedModels, model]);
    }
  };

  const needsKey = needsApiKey(provider);
  const canSave = needsKey ? apiKey.trim().length > 0 : true;

  return (
    <div>
      <h2 className="text-lg font-semibold">Configure LLM Provider</h2>
      <p className="mt-1 text-sm text-zinc-500">At least one provider is needed to chat with Agents</p>

      <div className="mt-6 space-y-4">
        {/* Provider selector */}
        <div className="rounded-md border border-zinc-200 p-4 dark:border-zinc-700">
          <div className="flex items-center gap-2">
            <span className="text-lg">🔑</span>
            <select
              value={provider}
              onChange={(e) => handleProviderChange(e.target.value)}
              className="w-full rounded-md border border-zinc-200 px-2 py-1 text-sm dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
            >
              {dynamicProviders.map((p) => (
                <option key={p.id} value={p.id}>{p.name}</option>
              ))}
            </select>
          </div>

          {/* API Key input */}
          {needsKey && (
            <input
              type="password"
              value={apiKey}
              onChange={(e) => { setApiKey(e.target.value); setSaved(false); }}
              placeholder={keyPlaceholder(provider)}
              className="mt-2 w-full rounded-md border border-zinc-200 px-3 py-2 text-sm dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
            />
          )}

          {/* Base URL input (if editable) */}
          {true && (
            <input
              type="text"
              value={baseUrl}
              onChange={(e) => { setBaseUrl(e.target.value); setSaved(false); }}
              placeholder="Base URL"
              className="mt-2 w-full rounded-md border border-zinc-200 px-3 py-2 text-xs font-mono dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
            />
          )}

          {/* Model selection (multi-select) */}
          <div className="mt-2">
            <label className="mb-1 block text-xs text-zinc-500">
              Default Model {selectedModels.length > 0 && <span className="text-accent-green">({selectedModels.length} selected)</span>}
            </label>
            {/* Selected model tags */}
            {selectedModels.length > 0 && (
              <div className="mb-1 flex flex-wrap gap-1">
                {selectedModels.map((m) => (
                  <span key={m} className="inline-flex items-center gap-1 rounded px-2 py-0.5 text-xs border" style={{ backgroundColor: "color-mix(in srgb, var(--color-accent) 10%, transparent)", color: "var(--color-accent)", borderColor: "color-mix(in srgb, var(--color-accent) 20%, transparent)" }}>
                    {m}
                    <button onClick={() => setSelectedModels(selectedModels.filter((x) => x !== m))} className="hover:opacity-70" style={{ color: "var(--color-accent)" }}>×</button>
                  </span>
                ))}
              </div>
            )}
            {/* Search and select */}
            <input
              type="text"
              value={modelSearchTerm}
              onChange={(e) => setModelSearchTerm(e.target.value)}
              placeholder="Search models..."
              className="w-full rounded-md border border-zinc-200 px-3 py-1.5 text-xs dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
            />
            <div className="mt-1 max-h-32 overflow-y-auto rounded border border-zinc-200 dark:border-zinc-700">
              {modelsLoading ? (
                <div className="px-3 py-2 text-xs text-zinc-400">Loading models...</div>
              ) : (
                availableModels
                  .filter((m) =>
                    !modelSearchTerm ||
                    m.id.toLowerCase().includes(modelSearchTerm.toLowerCase()) ||
                    m.name.toLowerCase().includes(modelSearchTerm.toLowerCase())
                  )
                  .map((m) => (
                    <label
                      key={m.id}
                      className="flex cursor-pointer items-center gap-2 px-3 py-1 text-xs hover:bg-zinc-50 dark:hover:bg-zinc-700"
                    >
                      <input
                        type="checkbox"
                        checked={selectedModels.includes(m.id)}
                        onChange={() => toggleModel(m.id)}
                        className="accent-[var(--color-accent)]"
                      />
                      <span className="flex-1 truncate">{m.name || m.id}</span>
                    </label>
                  ))
              )}
            </div>
            {/* Manual model input */}
            <input
              type="text"
              placeholder="Or type a custom model name and press Enter..."
              className="mt-1 w-full rounded-md border border-zinc-200 px-2 py-1 text-xs dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  const val = (e.target as HTMLInputElement).value.trim();
                  if (val && !selectedModels.includes(val)) {
                    setSelectedModels([...selectedModels, val]);
                    (e.target as HTMLInputElement).value = "";
                  }
                }
              }}
            />
          </div>



          <button
            onClick={handleSave}
            disabled={!canSave || saving}
            className="mt-2 rounded btn-solid px-3 py-1.5 text-xs font-medium disabled:opacity-50"
          >
            {saving ? "Saving..." : saved ? "Saved \u2713" : "Save"}
          </button>
        </div>

        {/* Local providers info */}
        <div className="rounded-md border border-zinc-200 p-4 dark:border-zinc-700">
          <div className="flex items-center gap-2">
            <span className="text-lg">🏠</span>
            <span className="text-sm font-medium">Local Providers (no key needed)</span>
          </div>
          <p className="mt-1 text-xs text-zinc-400">
            {dynamicProviders.filter(p => !needsApiKey(p.id)).map((p) => p.name).join(", ") || "Ollama, LM Studio"}
          </p>
        </div>
      </div>

      <div className="mt-8 flex justify-between">
        <button onClick={onPrev} className="rounded-md px-4 py-2 text-xs text-zinc-500 hover:text-zinc-700 dark:hover:text-zinc-300">
          Back
        </button>
        <button
          onClick={onNext}
          className="rounded btn-solid px-4 py-2 text-xs font-medium"
        >
          Next
        </button>
      </div>
    </div>
  );
}

/** Step 4: Identity */
function IdentityStep({
  name, language, timezone, city, occupation,
  onUpdate, onNext, onPrev,
}: {
  name: string; language: string; timezone: string; city: string; occupation: string;
  onUpdate: (updates: Partial<OnboardingState>) => void;
  onNext: () => void; onPrev: () => void;
}) {
  const requiredFilled = name.trim() && language && timezone;

  return (
    <div>
      <h2 className="text-lg font-semibold">Identity Information</h2>
      <p className="mt-1 text-sm text-zinc-500">Help Agents understand you better (required fields marked *)</p>

      <div className="mt-6 space-y-4">
        <div>
          <label className="mb-1 block text-xs text-zinc-500">Name *</label>
          <StyledInput
            type="text"
            value={name}
            onChange={(e) => onUpdate({ name: e.target.value })}
            placeholder="Your Name"
            className="rounded-md border border-zinc-200 px-3 py-2 text-sm dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
          />
        </div>
        <div>
          <label className="mb-1 block text-xs text-zinc-500">Language *</label>
          <select
            value={language}
            onChange={(e) => onUpdate({ language: e.target.value })}
            className="w-full rounded border border-zinc-200 bg-white px-2.5 py-1.5 text-xs text-zinc-800 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
            style={{
              backgroundImage: `url("data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg' fill='none' viewBox='0 0 20 20'%3e%3cpath stroke='%236b7280' stroke-linecap='round' stroke-linejoin='round' stroke-width='1.5' d='M6 8l4 4 4-4'/%3e%3c/svg%3e")`,
              backgroundPosition: 'right 0.5rem center',
              backgroundRepeat: 'no-repeat',
              backgroundSize: '1.5em 1.5em',
              paddingRight: '2rem',
              appearance: 'none',
              WebkitAppearance: 'none',
              MozAppearance: 'none',
            }}
          >
            <option value="zh-CN">中文 (简体)</option>
            <option value="zh-TW">中文 (繁體)</option>
            <option value="en">English</option>
            <option value="ja">日本語</option>
            <option value="ko">한국어</option>
          </select>
        </div>
        <div>
          <label className="mb-1 block text-xs text-zinc-500">Timezone *</label>
          <select
            value={timezone}
            onChange={(e) => onUpdate({ timezone: e.target.value })}
            className="w-full rounded border border-zinc-200 bg-white px-2.5 py-1.5 text-xs text-zinc-800 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
            style={{
              backgroundImage: `url("data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg' fill='none' viewBox='0 0 20 20'%3e%3cpath stroke='%236b7280' stroke-linecap='round' stroke-linejoin='round' stroke-width='1.5' d='M6 8l4 4 4-4'/%3e%3c/svg%3e")`,
              backgroundPosition: 'right 0.5rem center',
              backgroundRepeat: 'no-repeat',
              backgroundSize: '1.5em 1.5em',
              paddingRight: '2rem',
              appearance: 'none',
              WebkitAppearance: 'none',
              MozAppearance: 'none',
            }}
          >
            <option value="Asia/Shanghai">Asia/Shanghai</option>
            <option value="Asia/Tokyo">Asia/Tokyo</option>
            <option value="America/New_York">America/New_York</option>
            <option value="America/Los_Angeles">America/Los_Angeles</option>
            <option value="Europe/London">Europe/London</option>
            <option value="UTC">UTC</option>
          </select>
        </div>
        <div>
          <label className="mb-1 block text-xs text-zinc-500">City (optional)</label>
          <input
            type="text"
            value={city}
            onChange={(e) => onUpdate({ city: e.target.value })}
            className="w-full rounded-md border border-zinc-200 px-3 py-2 text-sm dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
          />
        </div>
        <div>
          <label className="mb-1 block text-xs text-zinc-500">Occupation (optional)</label>
          <input
            type="text"
            value={occupation}
            onChange={(e) => onUpdate({ occupation: e.target.value })}
            className="w-full rounded-md border border-zinc-200 px-3 py-2 text-sm dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
          />
        </div>
      </div>

      <div className="mt-8 flex justify-between">
        <button onClick={onPrev} className="rounded-md px-4 py-2 text-xs text-zinc-500 hover:text-zinc-700 dark:hover:text-zinc-300">
          Back
        </button>
        <button
          onClick={onNext}
          disabled={!requiredFilled}
          className="rounded-md bg-zinc-200 px-4 py-2 text-xs font-medium text-zinc-800 hover:bg-zinc-300 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
        >
          Next
        </button>
      </div>
    </div>
  );
}

/** Step 5: Install first Agent */
function InstallAgentStep({ onComplete, onPrev }: { onComplete: () => void; onPrev: () => void }) {
  const [installing, setInstalling] = useState<string | null>(null);
  const [installError, setInstallError] = useState<string | null>(null);
  const [selectedAgents, setSelectedAgents] = useState<string[]>(() => RECOMMENDED_AGENTS.map((agent) => agent.resourceName));

  const toggleRecommendedAgent = (resourceName: string) => {
    setSelectedAgents((prev) => prev.includes(resourceName)
      ? prev.filter((name) => name !== resourceName)
      : [...prev, resourceName]);
  };

  const handleInstallRecommended = async () => {
    setInstallError(null);
    try {
      for (const resourceName of selectedAgents) {
        const agent = RECOMMENDED_AGENTS.find((item) => item.resourceName === resourceName);
        setInstalling(agent?.name ?? resourceName);
        await invoke("install_bundled_agent", { resourceName, devMode: true });
      }
      // Refresh the agent list so newly installed agents appear.
      // ADR-017: Avatar assignment is now server-side (agent_config.json
      // seeded from manifest on first start). No frontend backfill needed.
      await useAgentStore.getState().fetchAgents();
      setInstalling(null);
    } catch (err) {
      setInstalling(null);
      setInstallError(err instanceof Error ? err.message : String(err));
    }
  };

  const handleInstallFromFile = async () => {
    try {
      const selected = await open({
        multiple: false,
        filters: [{ name: "Agent Package", extensions: ["agent"] }],
      });
      if (selected) {
        setInstallError(null);
        setInstalling(selected);
        await invoke("install_agent", { packagePath: selected });
        // Refresh the agent list so the backfill runs for the new agent.
        await useAgentStore.getState().fetchAgents();
        setInstalling(null);
      }
    } catch (err) {
      setInstalling(null);
      setInstallError(err instanceof Error ? err.message : String(err));
    }
  };

  return (
    <div>
      <h2 className="text-lg font-semibold">Install Your First Agent</h2>
      <p className="mt-1 text-sm text-zinc-500">Choose recommended built-in agents or install a local package</p>

      <div className="mt-6 space-y-3">
        <div className="rounded-md border border-zinc-200 p-4 dark:border-zinc-700">
          <div className="mb-3 flex items-center justify-between">
            <div>
              <h3 className="text-sm font-medium">Recommended Agents</h3>
              <p className="mt-1 text-xs text-zinc-400">Select one or more built-in agents to install</p>
            </div>
            <button
              onClick={() => setSelectedAgents(selectedAgents.length === RECOMMENDED_AGENTS.length ? [] : RECOMMENDED_AGENTS.map((agent) => agent.resourceName))}
              className="text-xs text-zinc-500 hover:text-zinc-800 dark:hover:text-zinc-200"
            >
              {selectedAgents.length === RECOMMENDED_AGENTS.length ? "Clear" : "Select all"}
            </button>
          </div>
          <div className="max-h-56 space-y-2 overflow-y-auto pr-1">
            {RECOMMENDED_AGENTS.map((agent) => (
              <label
                key={agent.resourceName}
                className="flex cursor-pointer gap-3 rounded-md border border-zinc-100 p-3 hover:bg-zinc-50 dark:border-zinc-800 dark:hover:bg-zinc-800"
              >
                <input
                  type="checkbox"
                  checked={selectedAgents.includes(agent.resourceName)}
                  onChange={() => toggleRecommendedAgent(agent.resourceName)}
                  className="mt-0.5 h-4 w-4 rounded border-zinc-300"
                />
                <span>
                  <span className="block text-sm font-medium">{agent.name} · {agent.role}</span>
                  <span className="mt-0.5 block text-xs text-zinc-400">{agent.description}</span>
                </span>
              </label>
            ))}
          </div>
          <button
            onClick={handleInstallRecommended}
            disabled={!!installing || selectedAgents.length === 0}
            className="mt-4 w-full rounded btn-solid py-2 text-xs font-medium disabled:opacity-50"
          >
            {installing ? `Installing ${installing}...` : `Install selected (${selectedAgents.length})`}
          </button>
        </div>

        <button
          onClick={handleInstallFromFile}
          disabled={!!installing}
          className="w-full rounded-md border border-zinc-200 p-4 text-left transition-colors hover:bg-zinc-50 dark:border-zinc-700 dark:hover:bg-zinc-800"
        >
          <span className="text-sm font-medium">Install from .agent file</span>
          <p className="mt-1 text-xs text-zinc-400">Select a .agent package from your computer</p>
        </button>

        {installing && (
          <p className="text-xs text-zinc-400">Installing: {installing}</p>
        )}
        {installError && (
          <p className="text-xs text-red-500">{installError}</p>
        )}
      </div>

      <div className="mt-8 flex justify-between">
        <button onClick={onPrev} className="rounded-md px-4 py-2 text-xs text-zinc-500 hover:text-zinc-700 dark:hover:text-zinc-300">
          Back
        </button>
        <button
          onClick={onComplete}
          className="rounded-md bg-zinc-200 px-4 py-2 text-xs font-medium text-zinc-800 hover:bg-zinc-300 dark:bg-zinc-700 dark:hover:bg-zinc-600"
        >
          Complete →
        </button>
      </div>
    </div>
  );
}

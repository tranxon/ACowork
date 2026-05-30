import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { useGatewayStore } from "../../stores/gatewayStore";
import { cn } from "../../lib/utils";
import { needsApiKey, keyPlaceholder } from "../../lib/providers";
import { fetchProviderModels, fetchProviders, createUser } from "../../lib/gateway-api";
import { DEFAULT_GATEWAY_URL } from "../../lib/config";
import type { ModelInfo } from "../../lib/types";

const TOTAL_STEPS = 5;

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
    const saved = localStorage.getItem("rollball_onboarding");
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
    }
    localStorage.setItem("rollball_onboarding", "completed");
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
      <div className="text-4xl">🎉</div>
      <h1 className="mt-4 text-2xl font-bold">Welcome to Rollball</h1>
      <p className="mt-2 text-sm text-zinc-500">Let's quickly set up your Agent environment</p>
      <div className="mt-8 space-y-3">
        <button
          onClick={onNext}
          className="w-full rounded-md btn-solid py-2.5 text-sm font-medium"
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

/** Step 2: Gateway Connection */
function GatewayStep({ onNext, onPrev }: { onNext: () => void; onPrev: () => void }) {
  const { status, checkHealth } = useGatewayStore();
  const [checking, setChecking] = useState(false);

  useEffect(() => {
    checkHealth();
  }, [checkHealth]);

  // Preload provider list as soon as Gateway is connected.
  // This triggers an early /api/models request, which starts the background
  // models.dev refresh (5+ second network call) while the user is still on
  // Step 2-4, so by the time they reach Step 3 (ApiKeyStep), the cache
  // may already be populated with fresh network data.
  useEffect(() => {
    if (status === "connected") {
      // Fire-and-forget — don't block the UI
      fetchProviders().catch(() => {
        // Ignore errors — offline fallback will be used in Step 3
      });
    }
  }, [status]);

  const handleRetry = async () => {
    setChecking(true);
    await checkHealth();
    setChecking(false);
  };

  return (
    <div>
      <h2 className="text-lg font-semibold">Connect to Gateway</h2>
      <p className="mt-1 text-sm text-zinc-500">Rollball needs a running Gateway to manage Agents</p>

      <div className="mt-6 space-y-4">
        <div>
          <label className="mb-1 block text-xs text-zinc-500">Gateway Address</label>
          <input
            type="text"
            value={DEFAULT_GATEWAY_URL}
            readOnly
            className="w-full rounded-md border border-zinc-200 bg-zinc-50 px-3 py-2 text-sm dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-300"
          />
        </div>

        <div className="flex items-center gap-2 text-sm">
          <span className="text-zinc-500">Status:</span>
          {checking ? (
            <span className="text-zinc-400">Checking...</span>
          ) : status === "connected" ? (
            <>
              <span className="h-2 w-2 rounded-full bg-green-500" />
              <span className="text-green-600 dark:text-green-400">Gateway Connected</span>
            </>
          ) : (
            <>
              <span className="h-2 w-2 rounded-full bg-red-500" />
              <span className="text-red-600 dark:text-red-400">Cannot connect to Gateway</span>
            </>
          )}
        </div>

        {status !== "connected" && !checking && (
          <p className="text-xs text-zinc-400">
            Please start Gateway: <code className="rounded bg-zinc-100 px-1 py-0.5 dark:bg-zinc-800">rollball-gateway --daemon</code>
          </p>
        )}

        {status !== "connected" && (
          <button onClick={handleRetry} className="text-xs text-zinc-500 hover:text-zinc-700 dark:hover:text-zinc-300">
            Retry
          </button>
        )}
      </div>

      <div className="mt-8 flex justify-between">
        <button onClick={onPrev} className="rounded-md px-4 py-2 text-xs text-zinc-500 hover:text-zinc-700 dark:hover:text-zinc-300">
          Back
        </button>
        <button
          onClick={onNext}
          disabled={status !== "connected"}
          className="rounded-md btn-solid px-4 py-2 text-xs font-medium disabled:opacity-50"
        >
          Next
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
        <div className="rounded-lg border border-zinc-200 p-4 dark:border-zinc-700">
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
            className="mt-2 rounded-md btn-solid px-3 py-1.5 text-xs font-medium disabled:opacity-50"
          >
            {saving ? "Saving..." : saved ? "Saved \u2713" : "Save"}
          </button>
        </div>

        {/* Local providers info */}
        <div className="rounded-lg border border-zinc-200 p-4 dark:border-zinc-700">
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
          className="rounded-md btn-solid px-4 py-2 text-xs font-medium"
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
          <input
            type="text"
            value={name}
            onChange={(e) => onUpdate({ name: e.target.value })}
            placeholder="Your name"
            className="w-full rounded-md border border-zinc-200 px-3 py-2 text-sm dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
          />
        </div>
        <div>
          <label className="mb-1 block text-xs text-zinc-500">Language *</label>
          <select
            value={language}
            onChange={(e) => onUpdate({ language: e.target.value })}
            className="w-full rounded-lg border border-zinc-200 bg-white px-2.5 py-1.5 text-xs text-zinc-800 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
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
            className="w-full rounded-lg border border-zinc-200 bg-white px-2.5 py-1.5 text-xs text-zinc-800 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
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

  const handleInstallFromFile = async () => {
    try {
      const selected = await open({
        multiple: false,
        filters: [{ name: "Agent Package", extensions: ["agent"] }],
      });
      if (selected) {
        setInstalling(selected);
        await invoke("install_agent", { packagePath: selected });
        setInstalling(null);
      }
    } catch {
      setInstalling(null);
    }
  };

  return (
    <div>
      <h2 className="text-lg font-semibold">Install Your First Agent</h2>

      <div className="mt-6 space-y-3">
        <button
          onClick={handleInstallFromFile}
          disabled={!!installing}
          className="w-full rounded-lg border border-zinc-200 p-4 text-left transition-colors hover:bg-zinc-50 dark:border-zinc-700 dark:hover:bg-zinc-800"
        >
          <span className="text-sm font-medium">📁 Install from .agent file</span>
          <p className="mt-1 text-xs text-zinc-400">Select a .agent package from your computer</p>
        </button>

        {installing && (
          <p className="text-xs text-zinc-400">Installing from: {installing}</p>
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

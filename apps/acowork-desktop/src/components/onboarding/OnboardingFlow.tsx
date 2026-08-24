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
import { ErrorBox } from "../common/ErrorBox";
import { Dropdown } from "../common/Dropdown";
import { useTranslation } from "../../i18n/useTranslation";
import { ModelMultiSelect } from "../harness/ModelMultiSelect";
import brandMark from "../../../../../assets/brand-mark.svg";
import { log } from "../../lib/logger";

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
  const { t } = useTranslation();
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
        log.warn("Failed to create user profile during onboarding:", err);
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
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-chat-area">
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
          <p className="mt-2 text-xs text-zinc-400">{t("onboarding.step", { current: state.currentStep, total: TOTAL_STEPS })}</p>
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
  const { t } = useTranslation();
  return (
    <div className="text-center">
      <div className="text-4xl">🎉🎉🎉</div>
      <h1 className="mt-4 text-2xl font-bold flex items-center justify-center gap-2"><span>{t("onboarding.welcome.title")}</span><img src={brandMark} alt="ACowork" className="h-10" /></h1>
      <p className="mt-2 text-sm text-zinc-500">{t("onboarding.welcome.subtitle")}</p>
      <div className="mt-8 space-y-3">
        <button
          onClick={onNext}
          className="w-full rounded btn-solid py-2.5 text-sm font-medium"
        >
          {t("onboarding.welcome.startSetup")}
        </button>
        <button
          onClick={onSkip}
          className="w-full py-2 text-xs text-zinc-400 hover:text-zinc-600 dark:hover:text-zinc-300"
        >
          {t("onboarding.welcome.skip")}
        </button>
      </div>
    </div>
  );
}

/** Step 2: Gateway Connection — mode selection + connect */
function GatewayStep({ onNext, onPrev }: { onNext: () => void; onPrev: () => void }) {
  const { t } = useTranslation();
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
      setStartError(t("onboarding.gateway.startFailedToast"));
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
      <h2 className="text-lg font-semibold">{t("onboarding.gateway.title")}</h2>
      <p className="mt-1 text-sm text-zinc-500">{t("onboarding.gateway.subtitle")}</p>

      <div className="mt-6 space-y-4">
        {/* Mode selection */}
        <div className="rounded-md border border-zinc-200 p-4 dark:border-zinc-700">
          <label className="mb-2 block text-xs text-zinc-500">{t("onboarding.gateway.modeLabel")}</label>
          <RadioGroup
            name="gatewayMode"
            value={gatewayMode}
            options={[
              { label: <span className="font-medium">{t("onboarding.gateway.modeLocalRecommended")}</span>, value: "local" as GatewayMode },
              { label: t("onboarding.gateway.modeRemote"), value: "remote" as GatewayMode },
            ]}
            onChange={handleModeChange}
          />
          {gatewayMode === "local" && (
            <p className="mt-1 text-xs text-zinc-400">
              {t("onboarding.gateway.localHint")}
            </p>
          )}
          {gatewayMode === "remote" && (
            <p className="mt-1 text-xs text-zinc-400">
              {t("onboarding.gateway.remoteHint")}
            </p>
          )}
        </div>

        {/* Local mode: auto-start status */}
        {gatewayMode === "local" && (
          <div className="rounded-md border border-zinc-200 p-4 dark:border-zinc-700">
            <div className="flex items-center gap-2 text-sm">
              <span className="text-zinc-500">{t("onboarding.gateway.status")}</span>
              {starting ? (
                <span className="text-zinc-400">{t("onboarding.gateway.starting")}</span>
              ) : localConnected ? (
                <>
                  <span className="h-2 w-2 rounded-full bg-green-500" />
                  <span className="text-green-600 dark:text-green-400">{t("onboarding.gateway.connected")}</span>
                </>
              ) : (
                <>
                  <span className="h-2 w-2 rounded-full bg-amber-500" />
                  <span className="text-amber-600 dark:text-amber-400">
                    {localError ? t("onboarding.gateway.failedToStart") : t("onboarding.gateway.notStarted")}
                  </span>
                </>
              )}
            </div>
            {localError && (
              <div className="mt-1">
                <ErrorBox message={startError ?? t("onboarding.gateway.startFailed")} />
              </div>
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
            <label className="mb-1 block text-xs text-zinc-500">{t("onboarding.gateway.urlLabel")}</label>
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
                  {t("onboarding.gateway.apply")}
                </button>
              )}
            </div>

            <div className="mt-3 flex items-center gap-2 text-sm">
              <span className="text-zinc-500">{t("onboarding.gateway.status")}</span>
              {checking ? (
                <span className="text-zinc-400">{t("onboarding.gateway.checking")}</span>
              ) : status === "connected" ? (
                <>
                  <span className="h-2 w-2 rounded-full bg-green-500" />
                  <span className="text-green-600 dark:text-green-400">{t("onboarding.gateway.connectedShort")}</span>
                </>
              ) : (
                <>
                  <span className="h-2 w-2 rounded-full bg-red-500" />
                  <span className="text-red-600 dark:text-red-400">{t("onboarding.gateway.cannotConnect")}</span>
                </>
              )}
            </div>

            {status !== "connected" && (
              <button
                onClick={handleTestRemote}
                disabled={checking || !urlDraft.trim()}
                className="mt-3 rounded btn-solid px-3 py-1.5 text-xs font-medium disabled:opacity-50"
              >
                {checking ? t("onboarding.gateway.startingShort") : t("onboarding.gateway.testConnection")}
              </button>
            )}
          </div>
        )}
      </div>

      <div className="mt-8 flex justify-between">
        <button onClick={onPrev} className="rounded-md px-4 py-2 text-xs text-zinc-500 hover:text-zinc-700 dark:hover:text-zinc-300">
          {t("onboarding.gateway.back")}
        </button>
        <button
          onClick={onNext}
          disabled={!canProceed || starting}
          className="rounded btn-solid px-4 py-2 text-xs font-medium disabled:opacity-50"
        >
          {starting ? t("onboarding.gateway.startingShort") : t("onboarding.gateway.next")}
        </button>
      </div>
    </div>
  );
}

/** Step 3: API Key */
function ApiKeyStep({ onNext, onPrev }: { onNext: () => void; onPrev: () => void }) {
  const { t } = useTranslation();
  const [provider, setProvider] = useState("openai");
  const [dynamicProviders, setDynamicProviders] = useState<Array<{ id: string; name: string; api?: string }>>([]);
  const [apiKey, setApiKey] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [selectedModels, setSelectedModels] = useState<string[]>([]);
  const [availableModels, setAvailableModels] = useState<ModelInfo[]>([]);
  const [modelsLoading, setModelsLoading] = useState(false);
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

  const needsKey = needsApiKey(provider);
  const canSave = needsKey ? apiKey.trim().length > 0 : true;

  return (
    <div>
      <h2 className="text-lg font-semibold">{t("onboarding.apiKey.title")}</h2>
      <p className="mt-1 text-sm text-zinc-500">{t("onboarding.apiKey.subtitle")}</p>

      <div className="mt-6 space-y-4">
        {/* Provider selector */}
        <div className="rounded-md border border-zinc-200 p-4 dark:border-zinc-700">
          <div className="flex items-center gap-2">
            <span className="text-lg">🔑</span>
            <Dropdown
              value={provider}
              onChange={(v) => handleProviderChange(v)}
              options={dynamicProviders.map((p) => ({ value: p.id, label: p.name }))}
            />
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
              placeholder={t("onboarding.apiKey.baseUrlPlaceholder")}
              className="mt-2 w-full rounded-md border border-zinc-200 px-3 py-2 text-xs font-mono dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
            />
          )}

          {/* Model selection (shared multi-select component) */}
          <div className="mt-2">
            <ModelMultiSelect
              models={availableModels}
              loading={modelsLoading}
              selected={selectedModels}
              onSelectedChange={setSelectedModels}
              showCapabilityFilter={true}
              showMetadata={false}
              showCustomInput={true}
              showModelCapEditor={false}
              showCompactModel={false}
            />
          </div>



          <button
            onClick={handleSave}
            disabled={!canSave || saving}
            className="mt-2 rounded btn-solid px-3 py-1.5 text-xs font-medium disabled:opacity-50"
          >
            {saving ? t("onboarding.apiKey.saving") : saved ? t("onboarding.apiKey.saved") : t("onboarding.apiKey.save")}
          </button>
        </div>

        {/* Local providers info */}
        <div className="rounded-md border border-zinc-200 p-4 dark:border-zinc-700">
          <div className="flex items-center gap-2">
            <span className="text-lg">🏠</span>
            <span className="text-sm font-medium">{t("onboarding.apiKey.localProvidersLabel")}</span>
          </div>
          <p className="mt-1 text-xs text-zinc-400">
            {dynamicProviders.filter(p => !needsApiKey(p.id)).map((p) => p.name).join(", ") || t("onboarding.apiKey.localProvidersFallback")}
          </p>
        </div>
      </div>

      <div className="mt-8 flex justify-between">
        <button onClick={onPrev} className="rounded-md px-4 py-2 text-xs text-zinc-500 hover:text-zinc-700 dark:hover:text-zinc-300">
          {t("onboarding.apiKey.back")}
        </button>
        <button
          onClick={onNext}
          className="rounded btn-solid px-4 py-2 text-xs font-medium"
        >
          {t("onboarding.apiKey.next")}
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
  const { t } = useTranslation();
  const requiredFilled = name.trim() && language && timezone;

  return (
    <div>
      <h2 className="text-lg font-semibold">{t("onboarding.identity.title")}</h2>
      <p className="mt-1 text-sm text-zinc-500">{t("onboarding.identity.subtitle")}</p>

      <div className="mt-6 space-y-4">
        <div>
          <label className="mb-1 block text-xs text-zinc-500">{t("onboarding.identity.nameLabel")}</label>
          <StyledInput
            type="text"
            value={name}
            onChange={(e) => onUpdate({ name: e.target.value })}
            placeholder={t("onboarding.identity.namePlaceholder")}
            className="rounded-md border border-zinc-200 px-3 py-2 text-sm dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
          />
        </div>
        <div>
          <label className="mb-1 block text-xs text-zinc-500">{t("onboarding.identity.languageLabel")}</label>
          <Dropdown
            value={language}
            onChange={(v) => onUpdate({ language: v })}
            options={[
              { value: "zh-CN", label: t("onboarding.identity.languages.zh-CN") },
              { value: "zh-TW", label: t("onboarding.identity.languages.zh-TW") },
              { value: "en", label: t("onboarding.identity.languages.en") },
              { value: "ja", label: t("onboarding.identity.languages.ja") },
              { value: "ko", label: t("onboarding.identity.languages.ko") },
            ]}
          />
        </div>
        <div>
          <label className="mb-1 block text-xs text-zinc-500">{t("onboarding.identity.timezoneLabel")}</label>
          <Dropdown
            value={timezone}
            onChange={(v) => onUpdate({ timezone: v })}
            options={[
              { value: "Asia/Shanghai", label: "Asia/Shanghai" },
              { value: "Asia/Tokyo", label: "Asia/Tokyo" },
              { value: "America/New_York", label: "America/New_York" },
              { value: "America/Los_Angeles", label: "America/Los_Angeles" },
              { value: "Europe/London", label: "Europe/London" },
              { value: "UTC", label: "UTC" },
            ]}
          />
        </div>
        <div>
          <label className="mb-1 block text-xs text-zinc-500">{t("onboarding.identity.cityLabel")}</label>
          <input
            type="text"
            value={city}
            onChange={(e) => onUpdate({ city: e.target.value })}
            className="w-full rounded-md border border-zinc-200 px-3 py-2 text-sm dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
          />
        </div>
        <div>
          <label className="mb-1 block text-xs text-zinc-500">{t("onboarding.identity.occupationLabel")}</label>
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
          {t("onboarding.identity.back")}
        </button>
        <button
          onClick={onNext}
          disabled={!requiredFilled}
          className="rounded-md bg-zinc-200 px-4 py-2 text-xs font-medium text-zinc-800 hover:bg-zinc-300 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
        >
          {t("onboarding.identity.next")}
        </button>
      </div>
    </div>
  );
}

/** Step 5: Install first Agent */
function InstallAgentStep({ onComplete, onPrev }: { onComplete: () => void; onPrev: () => void }) {
  const { t } = useTranslation();
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
      <h2 className="text-lg font-semibold">{t("onboarding.installAgent.title")}</h2>
      <p className="mt-1 text-sm text-zinc-500">{t("onboarding.installAgent.subtitle")}</p>

      <div className="mt-6 space-y-3">
        <div className="rounded-md border border-zinc-200 p-4 dark:border-zinc-700">
          <div className="mb-3 flex items-center justify-between">
            <div>
              <h3 className="text-sm font-medium">{t("onboarding.installAgent.recommendedTitle")}</h3>
              <p className="mt-1 text-xs text-zinc-400">{t("onboarding.installAgent.recommendedSubtitle")}</p>
            </div>
            <button
              onClick={() => setSelectedAgents(selectedAgents.length === RECOMMENDED_AGENTS.length ? [] : RECOMMENDED_AGENTS.map((agent) => agent.resourceName))}
              className="text-xs text-zinc-500 hover:text-zinc-800 dark:hover:text-zinc-200"
            >
              {selectedAgents.length === RECOMMENDED_AGENTS.length ? t("onboarding.installAgent.clear") : t("onboarding.installAgent.selectAll")}
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
            {installing ? t("onboarding.installAgent.installSelectedInstalling", { name: installing }) : t("onboarding.installAgent.installSelected", { count: selectedAgents.length })}
          </button>
        </div>

        <button
          onClick={handleInstallFromFile}
          disabled={!!installing}
          className="w-full rounded-md border border-zinc-200 p-4 text-left transition-colors hover:bg-zinc-50 dark:border-zinc-700 dark:hover:bg-zinc-800"
        >
          <span className="text-sm font-medium">{t("onboarding.installAgent.fromFileTitle")}</span>
          <p className="mt-1 text-xs text-zinc-400">{t("onboarding.installAgent.fromFileSubtitle")}</p>
        </button>

        {installing && (
          <p className="text-xs text-zinc-400">{t("onboarding.installAgent.installing", { name: installing })}</p>
        )}
        {installError && (
          <ErrorBox message={installError} />
        )}
      </div>

      <div className="mt-8 flex justify-between">
        <button onClick={onPrev} className="rounded-md px-4 py-2 text-xs text-zinc-500 hover:text-zinc-700 dark:hover:text-zinc-300">
          {t("onboarding.installAgent.back")}
        </button>
        <button
          onClick={onComplete}
          className="rounded-md bg-zinc-200 px-4 py-2 text-xs font-medium text-zinc-800 hover:bg-zinc-300 dark:bg-zinc-700 dark:hover:bg-zinc-600"
        >
          {t("onboarding.installAgent.complete")}
        </button>
      </div>
    </div>
  );
}

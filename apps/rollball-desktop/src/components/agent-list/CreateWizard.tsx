import { useState, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { cn } from "../../lib/utils";
import {
  Sparkles,
  Bot,
  Layout,
  Check,
  Loader2,
  FileText,
  Globe,
} from "lucide-react";

interface CreateWizardProps {
  open: boolean;
  onCreated: (agentId: string) => void;
  onClose: () => void;
}

type WizardStep = "basic" | "llm" | "template" | "preview";

const STEPS: { key: WizardStep; label: string; icon: React.ElementType }[] = [
  { key: "basic", label: "Basic", icon: Bot },
  { key: "llm", label: "LLM", icon: Sparkles },
  { key: "template", label: "Template", icon: Layout },
  { key: "preview", label: "Preview", icon: Check },
];

interface AgentFormData {
  agent_id: string;
  name: string;
  version: string;
  description: string;
  author: string;
  suggested_provider: string;
  suggested_model: string;
}

const DEFAULT_FORM: AgentFormData = {
  agent_id: "",
  name: "",
  version: "0.1.0",
  description: "",
  author: "",
  suggested_provider: "openai",
  suggested_model: "gpt-4o",
};

const TEMPLATES = [
  {
    id: "blank",
    name: "Blank",
    desc: "Start from scratch with a minimal agent.",
    icon: FileText,
  },
  {
    id: "assistant",
    name: "Assistant",
    desc: "General-purpose assistant with tool calling.",
    icon: Bot,
    provider: "openai",
    model: "gpt-4o",
  },
  {
    id: "local",
    name: "Local LLM",
    desc: "Use Ollama or other local models.",
    icon: Globe,
    provider: "ollama",
    model: "qwen2.5:7b",
  },
];

export function CreateWizard({ open, onCreated, onClose }: CreateWizardProps) {
  const [step, setStep] = useState<WizardStep>("basic");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [form, setForm] = useState<AgentFormData>({ ...DEFAULT_FORM });

  // Reset on open
  useEffect(() => {
    if (open) {
      setStep("basic");
      setError(null);
      setForm({ ...DEFAULT_FORM });
    }
  }, [open]);

  // Close on Escape
  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !busy) onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [open, busy, onClose]);

  const update = (patch: Partial<AgentFormData>) =>
    setForm((prev) => ({ ...prev, ...patch }));

  const stepIndex = STEPS.findIndex((s) => s.key === step);
  const canNext = () => {
    switch (step) {
      case "basic":
        return form.agent_id.trim() !== "" && form.name.trim() !== "";
      default:
        return true;
    }
  };

  const handleNext = () => {
    if (step === "preview") {
      handleCreate();
      return;
    }
    const nextIdx = stepIndex + 1;
    if (nextIdx < STEPS.length) setStep(STEPS[nextIdx].key);
  };

  const handleBack = () => {
    const prevIdx = stepIndex - 1;
    if (prevIdx >= 0) setStep(STEPS[prevIdx].key);
  };

  const handleCreate = async () => {
    setBusy(true);
    setError(null);
    try {
      const agentId = await invoke<string>("create_agent", {
        agentId: form.agent_id.trim(),
        name: form.name.trim(),
        version: form.version || null,
        description: form.description || null,
        author: form.author || null,
        suggestedProvider: form.suggested_provider || null,
        suggestedModel: form.suggested_model || null,
      });
      onCreated(agentId);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  if (!open) return null;

  const nextLabel = step === "preview" ? "Create Agent" : "Next";
  const canProceed = canNext() && !busy;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      {/* Backdrop */}
      <div
        className="absolute inset-0 bg-black/40"
        onClick={busy ? undefined : onClose}
      />

      {/* Dialog */}
      <div className="relative z-10 flex w-full max-w-2xl flex-col rounded-lg border border-zinc-200 bg-white shadow-xl dark:border-zinc-700 dark:bg-zinc-800">
        {/* Header */}
        <div className="flex items-center gap-2 border-b border-zinc-200 px-5 py-3.5 dark:border-zinc-700">
          <Sparkles className="h-5 w-5 text-zinc-500 dark:text-zinc-400" />
          <h2 className="text-sm font-semibold text-zinc-800 dark:text-zinc-100">
            Create New Agent
          </h2>
        </div>

        {/* Step indicators */}
        <div className="flex items-center gap-0 border-b border-zinc-200 px-5 py-3 dark:border-zinc-700">
          {STEPS.map((s, i) => {
            const Icon = s.icon;
            const active = s.key === step;
            const passed = i < stepIndex;
            return (
              <div key={s.key} className="flex items-center">
                <div
                  className={cn(
                    "flex items-center gap-1.5 rounded-full px-2.5 py-1 text-xs font-medium transition-colors",
                    active &&
                      "bg-zinc-200 text-zinc-800 dark:bg-zinc-300 dark:text-zinc-900",
                    passed &&
                      "bg-green-100 text-green-700 dark:bg-green-900/30 dark:text-green-400",
                    !active && !passed && "text-zinc-400 dark:text-zinc-500",
                  )}
                >
                  {passed ? (
                    <Check className="h-3 w-3" />
                  ) : (
                    <Icon className="h-3 w-3" />
                  )}
                  {s.label}
                </div>
                {i < STEPS.length - 1 && (
                  <div
                    className={cn(
                      "mx-1 h-px w-4",
                      i < stepIndex
                        ? "bg-green-300 dark:bg-green-600"
                        : "bg-zinc-200 dark:bg-zinc-600",
                    )}
                  />
                )}
              </div>
            );
          })}
        </div>

        {/* Step content */}
        <div className="flex-1 space-y-4 overflow-y-auto px-5 py-4">
          {/* Step 1: Basic info */}
          {step === "basic" && (
            <div className="space-y-3">
              <div>
                <label className="mb-1 block text-xs font-medium text-zinc-500 dark:text-zinc-400">
                  Agent ID * <span className="font-normal text-zinc-400">(e.g. com.example.myagent)</span>
                </label>
                <input
                  type="text"
                  value={form.agent_id}
                  onChange={(e) => update({ agent_id: e.target.value })}
                  placeholder="com.example.myagent"
                  className="w-full rounded-md border border-zinc-200 bg-white px-3 py-1.5 text-xs text-zinc-800 placeholder-zinc-400 outline-none transition-colors focus:border-zinc-400 focus:ring-1 focus:ring-zinc-400 dark:border-zinc-600 dark:bg-zinc-700 dark:text-zinc-200 dark:placeholder-zinc-500"
                />
              </div>
              <div>
                <label className="mb-1 block text-xs font-medium text-zinc-500 dark:text-zinc-400">
                  Display Name *
                </label>
                <input
                  type="text"
                  value={form.name}
                  onChange={(e) => update({ name: e.target.value })}
                  placeholder="My Agent"
                  className="w-full rounded-md border border-zinc-200 bg-white px-3 py-1.5 text-xs text-zinc-800 placeholder-zinc-400 outline-none transition-colors focus:border-zinc-400 focus:ring-1 focus:ring-zinc-400 dark:border-zinc-600 dark:bg-zinc-700 dark:text-zinc-200 dark:placeholder-zinc-500"
                />
              </div>
              <div className="grid grid-cols-2 gap-3">
                <div>
                  <label className="mb-1 block text-xs font-medium text-zinc-500 dark:text-zinc-400">
                    Version
                  </label>
                  <input
                    type="text"
                    value={form.version}
                    onChange={(e) => update({ version: e.target.value })}
                    placeholder="0.1.0"
                    className="w-full rounded-md border border-zinc-200 bg-white px-3 py-1.5 text-xs text-zinc-800 placeholder-zinc-400 outline-none transition-colors focus:border-zinc-400 focus:ring-1 focus:ring-zinc-400 dark:border-zinc-600 dark:bg-zinc-700 dark:text-zinc-200 dark:placeholder-zinc-500"
                  />
                </div>
                <div>
                  <label className="mb-1 block text-xs font-medium text-zinc-500 dark:text-zinc-400">
                    Author
                  </label>
                  <input
                    type="text"
                    value={form.author}
                    onChange={(e) => update({ author: e.target.value })}
                    placeholder="Your Name"
                    className="w-full rounded-md border border-zinc-200 bg-white px-3 py-1.5 text-xs text-zinc-800 placeholder-zinc-400 outline-none transition-colors focus:border-zinc-400 focus:ring-1 focus:ring-zinc-400 dark:border-zinc-600 dark:bg-zinc-700 dark:text-zinc-200 dark:placeholder-zinc-500"
                  />
                </div>
              </div>
              <div>
                <label className="mb-1 block text-xs font-medium text-zinc-500 dark:text-zinc-400">
                  Description
                </label>
                <textarea
                  value={form.description}
                  onChange={(e) => update({ description: e.target.value })}
                  placeholder="Describe what this agent does..."
                  rows={3}
                  className="w-full resize-none rounded-md border border-zinc-200 bg-white px-3 py-1.5 text-xs text-zinc-800 placeholder-zinc-400 outline-none transition-colors focus:border-zinc-400 focus:ring-1 focus:ring-zinc-400 dark:border-zinc-600 dark:bg-zinc-700 dark:text-zinc-200 dark:placeholder-zinc-500"
                />
              </div>
            </div>
          )}

          {/* Step 2: LLM config */}
          {step === "llm" && (
            <div className="space-y-3">
              <div>
                <label className="mb-1 block text-xs font-medium text-zinc-500 dark:text-zinc-400">
                  Provider
                </label>
                <select
                  value={form.suggested_provider}
                  onChange={(e) =>
                    update({ suggested_provider: e.target.value })
                  }
                  className="w-full rounded-md border border-zinc-200 bg-white px-3 py-1.5 text-xs text-zinc-800 outline-none transition-colors focus:border-zinc-400 focus:ring-1 focus:ring-zinc-400 dark:border-zinc-600 dark:bg-zinc-700 dark:text-zinc-200"
                >
                  <option value="openai">OpenAI</option>
                  <option value="anthropic">Anthropic</option>
                  <option value="ollama">Ollama (Local)</option>
                  <option value="deepseek">DeepSeek</option>
                  <option value="groq">Groq</option>
                </select>
              </div>
              <div>
                <label className="mb-1 block text-xs font-medium text-zinc-500 dark:text-zinc-400">
                  Model
                </label>
                <input
                  type="text"
                  value={form.suggested_model}
                  onChange={(e) => update({ suggested_model: e.target.value })}
                  placeholder="gpt-4o"
                  className="w-full rounded-md border border-zinc-200 bg-white px-3 py-1.5 text-xs text-zinc-800 placeholder-zinc-400 outline-none transition-colors focus:border-zinc-400 focus:ring-1 focus:ring-zinc-400 dark:border-zinc-600 dark:bg-zinc-700 dark:text-zinc-200 dark:placeholder-zinc-500"
                />
              </div>
            </div>
          )}

          {/* Step 3: Template */}
          {step === "template" && (
            <div className="space-y-3">
              <p className="text-xs text-zinc-500 dark:text-zinc-400">
                Choose a starting template. This sets default provider and model settings.
              </p>
              <div className="grid gap-3">
                {TEMPLATES.map((tmpl) => {
                  const Icon = tmpl.icon;
                  return (
                    <button
                      key={tmpl.id}
                      onClick={() => {
                        if (tmpl.provider)
                          update({ suggested_provider: tmpl.provider });
                        if (tmpl.model)
                          update({ suggested_model: tmpl.model });
                      }}
                      className="flex items-start gap-3 rounded-md border border-zinc-200 px-3 py-2 text-left transition-colors hover:border-zinc-400 hover:bg-zinc-50 dark:border-zinc-600 dark:hover:border-zinc-400 dark:hover:bg-zinc-700"
                    >
                      <Icon className="mt-0.5 h-5 w-5 text-zinc-400" />
                      <div>
                        <p className="text-xs font-medium text-zinc-700 dark:text-zinc-200">
                          {tmpl.name}
                        </p>
                        <p className="text-xs text-zinc-500 dark:text-zinc-400">
                          {tmpl.desc}
                        </p>
                        {tmpl.provider && (
                          <p className="mt-1 text-xs text-zinc-400">
                            {tmpl.provider} / {tmpl.model}
                          </p>
                        )}
                      </div>
                    </button>
                  );
                })}
              </div>
            </div>
          )}

          {/* Step 4: Preview */}
          {step === "preview" && (
            <div className="space-y-3">
              <h3 className="text-xs font-medium text-zinc-700 dark:text-zinc-200">
                Review Your Agent
              </h3>
              <div className="rounded-md bg-zinc-50 px-4 py-3 dark:bg-zinc-700/50">
                <pre className="text-xs text-zinc-600 dark:text-zinc-300">
                  {`[package]
agent_id = "${form.agent_id}"
name = "${form.name}"
version = "${form.version}"
description = "${form.description || "(none)"}"
author = "${form.author || "(none)"}"
runtime_version = "0.1.0"
dev = true

[llm]
suggested_provider = "${form.suggested_provider}"
suggested_model = "${form.suggested_model}"
`}
                </pre>
              </div>
            </div>
          )}

          {/* Error */}
          {error && (
            <div className="rounded-md bg-red-50 px-3 py-2 text-xs text-red-600 dark:bg-red-900/20 dark:text-red-400">
              {error}
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="flex justify-between border-t border-zinc-200 px-5 py-3 dark:border-zinc-700">
          <button
            onClick={stepIndex === 0 ? onClose : handleBack}
            disabled={busy}
            className="rounded-md px-3 py-1.5 text-xs font-medium text-zinc-600 hover:bg-zinc-100 disabled:opacity-50 dark:text-zinc-400 dark:hover:bg-zinc-700"
          >
            {stepIndex === 0 ? "Cancel" : "Back"}
          </button>

          <button
            onClick={handleNext}
            disabled={!canProceed}
            className="flex items-center gap-2 rounded-md btn-solid px-3 py-1.5 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50"
          >
            {busy ? (
              <>
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
                Creating...
              </>
            ) : (
              nextLabel
            )}
          </button>
        </div>
      </div>
    </div>
  );
}

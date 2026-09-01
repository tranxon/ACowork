import { useState, useEffect, useCallback, useRef } from "react";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useTranslation } from "../../i18n/useTranslation";
import type { EmbeddingModelWithStatus, SelectModelMigrationResponse, CloudEmbeddingProvider, ActiveCloudEmbeddingProvider, CloudEmbeddingProvidersResponse } from "../../lib/types";
import { cn } from "../../lib/utils";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { ErrorBox } from "../common/ErrorBox";
import { Dropdown } from "../common/Dropdown";
import { fetchEmbeddingModels, downloadEmbeddingModel, selectEmbeddingModel, fetchEmbeddingModelStatus, testEmbeddingModel, deleteEmbeddingModel, startMigration, selectEmbeddingModelWithMigration } from "../../lib/gateway-api";
import { fetchCloudEmbeddingProviders, selectCloudEmbeddingModel, setCloudEmbeddingApiKey, deleteCloudEmbeddingApiKey, testCloudEmbeddingProvider, addCloudEmbeddingProvider } from "../../lib/gateway-api";
import type { EmbeddingTestResponse } from "../../lib/types";
import { Download, Check, Loader2, Cpu, Languages, Zap, CheckCircle2, XCircle, Trash2, Cloud, KeyRound, HardDrive, Plus } from "lucide-react";

export function EmbeddingModelTab() {
    const { t } = useTranslation();
    const status = useGatewayStore((s) => s.status);
    const migrationProgress = useGatewayStore((s) => s.migrationProgress);
    const pollMigrationProgress = useGatewayStore((s) => s.pollMigrationProgress);
    const [models, setModels] = useState<EmbeddingModelWithStatus[]>([]);
    const [activeModelId, setActiveModelId] = useState<string | null>(null);
    const [serviceRunning, setServiceRunning] = useState(false);
    const [loading, setLoading] = useState(false);
    const [downloadingIds, setDownloadingIds] = useState<Set<string>>(new Set());
    const [selectingId, setSelectingId] = useState<string | null>(null);
    const [deletingId, setDeletingId] = useState<string | null>(null);
    const [deleteConfirm, setDeleteConfirm] = useState<{ modelId: string; modelName: string } | null>(null);
    const [downloadProgress, setDownloadProgress] = useState<Record<string, number>>({});
    const [error, setError] = useState<string | null>(null);
    const [dimensionConfirm, setDimensionConfirm] = useState<{ modelId: string; message: string } | null>(null);
    const [migrationResponse, setMigrationResponse] = useState<SelectModelMigrationResponse | null>(null);
    const [migrationAgentIds, setMigrationAgentIds] = useState<Set<string>>(new Set());
    const [migrationStarting, setMigrationStarting] = useState(false);
    const [migrationStarted, setMigrationStarted] = useState(false);
    const migrationPollingRef = useRef<ReturnType<typeof setInterval> | null>(null);
    const [testing, setTesting] = useState(false);
    const [testResult, setTestResult] = useState<EmbeddingTestResponse | null>(null);

    // ── Cloud embedding providers (S1-7) ─────────────────────────────
    const [cloudProviders, setCloudProviders] = useState<CloudEmbeddingProvider[]>([]);
    const [cloudActive, setCloudActive] = useState<ActiveCloudEmbeddingProvider | null>(null);
    const [cloudLoading, setCloudLoading] = useState(false);
    const [cloudError, setCloudError] = useState<string | null>(null);
    const [cloudSelecting, setCloudSelecting] = useState<string | null>(null);
    const [keyEditingProvider, setKeyEditingProvider] = useState<string | null>(null);
    const [keyDraft, setKeyDraft] = useState("");
    const [keySaving, setKeySaving] = useState(false);
    const [cloudTesting, setCloudTesting] = useState<string | null>(null);
    // Per-provider test result map — each card renders ONLY its own result.
    // (Was a single global state which caused one provider's failure to bleed
    //  into every other card's UI.)
    const [cloudTestResults, setCloudTestResults] = useState<
        Record<string, EmbeddingTestResponse>
    >({});

    // Add custom cloud embedding provider dialog (S2)
    const [customDialogOpen, setCustomDialogOpen] = useState(false);

    const loadModels = useCallback(async () => {
        setLoading(true);
        setError(null);
        try {
            const resp = await fetchEmbeddingModels();
            setModels(resp.models);
            setActiveModelId(resp.active_model_id);
            setServiceRunning(resp.service_running);
        } catch (e) {
            setError(e instanceof Error ? e.message : "Failed to load models");
        } finally {
            setLoading(false);
        }
    }, []);

    useEffect(() => {
        if (status === "connected") {
            loadModels();
        }
    }, [status, loadModels]);

    useEffect(() => {
        if (status !== "connected" || !serviceRunning || activeModelId) return;
        const timer = setInterval(loadModels, 2000);
        return () => clearInterval(timer);
    }, [status, serviceRunning, activeModelId, loadModels]);

    useEffect(() => {
        const downloading = models.filter((model) => model.status === "downloading");
        const downloadingSet = new Set(downloading.map((model) => model.id));

        setDownloadingIds((prev) => {
            const next = new Set(prev);
            for (const model of downloading) next.add(model.id);
            return next;
        });
        setDownloadProgress((prev) => {
            const next = { ...prev };
            for (const model of downloading) {
                if (next[model.id] == null) next[model.id] = 0;
            }
            for (const model of models) {
                if (model.status !== "downloading" && !downloadingSet.has(model.id)) {
                    delete next[model.id];
                }
            }
            return next;
        });
    }, [models]);

    const handleDownload = useCallback(async (modelId: string, variant?: string) => {
        setDownloadingIds((prev) => new Set(prev).add(modelId));
        setDownloadProgress((prev) => ({ ...prev, [modelId]: 0 }));
        setError(null);
        try {
            await downloadEmbeddingModel(modelId, variant);
            // Fire-and-forget: response is immediate, polling handles progress
        } catch (e) {
            setError(e instanceof Error ? e.message : "Download failed");
            setDownloadingIds((prev) => {
                const next = new Set(prev);
                next.delete(modelId);
                return next;
            });
        }
    }, []);

    // Poll download progress for all in-flight downloads
    const pollingRef = useRef<ReturnType<typeof setInterval> | null>(null);
    useEffect(() => {
        if (downloadingIds.size === 0) {
            if (pollingRef.current) clearInterval(pollingRef.current);
            pollingRef.current = null;
            return;
        }

        let needsRefresh = false;

        const poll = async () => {
            // Snapshot current downloading ids to avoid stale closure
            const ids = Array.from(downloadingIds);

            // Query ALL model statuses in parallel to avoid serial round-trips
            const results = await Promise.allSettled(
                ids.map((id) => fetchEmbeddingModelStatus(id)),
            );

            const completedIds: string[] = [];
            const failedIds: string[] = [];

            results.forEach((result, i) => {
                if (result.status !== "fulfilled") return;
                const resp = result.value;
                const id = ids[i];

                if (resp.status === "downloading") {
                    // Update progress without deleting — avoid flicker
                    setDownloadProgress((prev) => ({ ...prev, [id]: resp.progress ?? 0 }));
                } else if (resp.status === "downloaded" || resp.status === "loaded") {
                    completedIds.push(id);
                } else if (resp.status === "failed") {
                    failedIds.push(id);
                }
            });

            // Report errors for any failed downloads
            for (const id of failedIds) {
                const idx = ids.indexOf(id);
                const result = results[idx];
                if (result.status === "fulfilled" && result.value.status === "failed") {
                    setError(`Download failed: ${result.value.error ?? "unknown error"}`);
                }
            }

            // Remove completed/failed IDs in a single batch to avoid
            // intermediate renders that wipe other models' progress
            if (completedIds.length > 0 || failedIds.length > 0) {
                const removeSet = new Set([...completedIds, ...failedIds]);

                setDownloadingIds((prev) => {
                    const next = new Set(prev);
                    for (const id of removeSet) next.delete(id);
                    return next;
                });

                // Set completed models to 100% before removing so user sees
                // the bar reach the end instead of jumping back to 0
                setDownloadProgress((prev) => {
                    const next = { ...prev };
                    for (const id of completedIds) next[id] = 100;
                    return next;
                });

                // Clean up progress after a short delay so the 100% bar renders
                setTimeout(() => {
                    setDownloadProgress((prev) => {
                        const next = { ...prev };
                        for (const id of removeSet) delete next[id];
                        return next;
                    });
                }, 400);

                needsRefresh = true;
            }

            // Refresh model list ONCE after all statuses are processed
            if (needsRefresh) {
                needsRefresh = false;
                await loadModels();
            }
        };

        poll(); // immediate first poll
        pollingRef.current = setInterval(poll, 2000);

        return () => {
            if (pollingRef.current) clearInterval(pollingRef.current);
            pollingRef.current = null;
        };
    }, [downloadingIds, loadModels]);

    const handleSelect = useCallback(async (modelId: string, force = false) => {
        setSelectingId(modelId);
        setError(null);
        try {
            if (force) {
                // Use migration-aware endpoint for forced selects
                const result = await selectEmbeddingModelWithMigration(modelId, force);
                if ("agents" in result && result.status === "migration_required") {
                    // Dimension changed — show migration agent list
                    setMigrationResponse(result);
                    setMigrationAgentIds(new Set(result.agents.filter(a => a.is_running).map(a => a.agent_id)));
                    setSelectingId(null);
                    await loadModels();
                    return;
                }
                // Same dimension or simple loaded response
                if (result.status === "loaded" || result.status === "migration_started") {
                    setMigrationResponse(null);
                    setMigrationStarted(false);
                }
                await loadModels();
            } else {
                const result = await selectEmbeddingModel(modelId, force);
                if (result.status === "dimension_mismatch") {
                    setDimensionConfirm({ modelId, message: result.message });
                    return;
                }
                await loadModels();
            }
        } catch (e) {
            setError(e instanceof Error ? e.message : "Select failed");
        } finally {
            setSelectingId(null);
        }
    }, [loadModels]);

    const handleDimensionConfirm = useCallback(async () => {
        if (!dimensionConfirm) return;
        setDimensionConfirm(null);
        await handleSelect(dimensionConfirm.modelId, true);
    }, [dimensionConfirm, handleSelect]);

    const handleStartMigration = useCallback(async () => {
        if (!migrationResponse || migrationAgentIds.size === 0) return;
        const modelId = migrationResponse.model_id;
        setMigrationStarting(true);
        setError(null);
        try {
            const agentIds = Array.from(migrationAgentIds);
            await startMigration(modelId, agentIds);
            setMigrationStarted(true);
            // Start polling migration progress
            if (migrationPollingRef.current) clearInterval(migrationPollingRef.current);
            migrationPollingRef.current = setInterval(async () => {
                const inProgress = await pollMigrationProgress();
                if (!inProgress) {
                    if (migrationPollingRef.current) {
                        clearInterval(migrationPollingRef.current);
                        migrationPollingRef.current = null;
                    }
                    setMigrationStarted(false);
                    await loadModels();
                }
            }, 2000);
        } catch (e) {
            setError(e instanceof Error ? e.message : "Migration start failed");
        } finally {
            setMigrationStarting(false);
        }
    }, [migrationResponse, migrationAgentIds, pollMigrationProgress, loadModels]);

    const handleMigrationCancel = useCallback(() => {
        setMigrationResponse(null);
        setMigrationAgentIds(new Set());
        setMigrationStarted(false);
        if (migrationPollingRef.current) {
            clearInterval(migrationPollingRef.current);
            migrationPollingRef.current = null;
        }
    }, []);

    const toggleMigrationAgent = useCallback((agentId: string) => {
        setMigrationAgentIds((prev) => {
            const next = new Set(prev);
            if (next.has(agentId)) {
                next.delete(agentId);
            } else {
                next.add(agentId);
            }
            return next;
        });
    }, []);

    const handleTest = useCallback(async () => {
        setTesting(true);
        setTestResult(null);
        try {
            const result = await testEmbeddingModel();
            setTestResult(result);
        } catch (e) {
            setTestResult({
                success: false,
                error: e instanceof Error ? e.message : "Test failed",
            });
        } finally {
            setTesting(false);
        }
    }, []);

    // ── Cloud embedding handlers (S1-7) ──────────────────────────────
    const loadCloudProviders = useCallback(async () => {
        setCloudLoading(true);
        setCloudError(null);
        try {
            const resp: CloudEmbeddingProvidersResponse = await fetchCloudEmbeddingProviders();
            setCloudProviders(resp.providers);
            setCloudActive(resp.active);
        } catch (e) {
            setCloudError(e instanceof Error ? e.message : "Failed to load cloud providers");
        } finally {
            setCloudLoading(false);
        }
    }, []);

    // Load cloud providers when Gateway connects (separate effect so the
    // `loadCloudProviders` callback is declared before use).
    useEffect(() => {
        if (status === "connected") {
            loadCloudProviders();
        }
    }, [status, loadCloudProviders]);

    const handleCloudSelect = useCallback(
        async (providerId: string, modelId: string) => {
            const key = `${providerId}/${modelId}`;
            setCloudSelecting(key);
            setCloudError(null);
            try {
                await selectCloudEmbeddingModel(providerId, modelId);
                await loadCloudProviders();
            } catch (e) {
                setCloudError(e instanceof Error ? e.message : "Select failed");
            } finally {
                setCloudSelecting(null);
            }
        },
        [loadCloudProviders],
    );

    const handleCloudKeySubmit = useCallback(
        async (providerId: string) => {
            if (!keyDraft.trim()) return;
            setKeySaving(true);
            setCloudError(null);
            try {
                await setCloudEmbeddingApiKey(providerId, keyDraft.trim());
                setKeyEditingProvider(null);
                setKeyDraft("");
                await loadCloudProviders();
                // After the key is persisted, fire a verification ping so
                // the user sees whether the provider accepted it. Backend
                // POST /api/embedding-providers/{id}/test returns
                // `{ ok, dimension, message }`; we map into the frontend
                // EmbeddingTestResponse shape (`success`, `error`) so the
                // existing inline UI can render success / failure.
                setCloudTesting(providerId);
                try {
                    const resp = await testCloudEmbeddingProvider(providerId);
                    setCloudTestResults((prev) => ({
                        ...prev,
                        [providerId]: {
                            success: resp.ok,
                            model_id: resp.model_id,
                            dimension: resp.dimension,
                            error: resp.ok ? null : resp.message,
                        },
                    }));
                } catch (e) {
                    setCloudTestResults((prev) => ({
                        ...prev,
                        [providerId]: {
                            success: false,
                            error: e instanceof Error ? e.message : "Verification failed",
                        },
                    }));
                } finally {
                    setCloudTesting(null);
                }
            } catch (e) {
                setCloudError(e instanceof Error ? e.message : "Failed to save API key");
            } finally {
                setKeySaving(false);
            }
        },
        [keyDraft, loadCloudProviders],
    );

    const handleCloudKeyDelete = useCallback(
        async (providerId: string) => {
            setCloudError(null);
            try {
                await deleteCloudEmbeddingApiKey(providerId);
                await loadCloudProviders();
            } catch (e) {
                setCloudError(e instanceof Error ? e.message : "Failed to remove API key");
            }
        },
        [loadCloudProviders],
    );

    const handleCloudTest = useCallback(async (providerId: string) => {
        setCloudTesting(providerId);
        // Clear only this provider's previous result (don't blow away others')
        setCloudTestResults((prev) => {
            if (!(providerId in prev)) return prev;
            const next = { ...prev };
            delete next[providerId];
            return next;
        });
        try {
            const result = await testCloudEmbeddingProvider(providerId);
            // Map CloudEmbeddingTestResponse → EmbeddingTestResponse so the
            // shared UI shape (success/error) renders uniformly.
            setCloudTestResults((prev) => ({
                ...prev,
                [providerId]: {
                    success: result.ok,
                    model_id: result.model_id,
                    dimension: result.dimension ?? null,
                    error: result.ok ? null : result.message ?? null,
                },
            }));
        } catch (e) {
            setCloudTestResults((prev) => ({
                ...prev,
                [providerId]: {
                    success: false,
                    error: e instanceof Error ? e.message : "Test failed",
                },
            }));
        } finally {
            setCloudTesting(null);
        }
    }, []);

    const handleDelete = useCallback(async (modelId: string) => {
        setDeletingId(modelId);
        setError(null);
        try {
            await deleteEmbeddingModel(modelId);
            await loadModels();
        } catch (e) {
            setError(e instanceof Error ? e.message : "Delete failed");
        } finally {
            setDeletingId(null);
        }
    }, [loadModels]);

    const handleDeleteConfirm = useCallback(async () => {
        if (!deleteConfirm) return;
        const id = deleteConfirm.modelId;
        setDeleteConfirm(null);
        await handleDelete(id);
    }, [deleteConfirm, handleDelete]);

    if (status !== "connected") {
        return (
            <div className="max-w-lg">
                <p className="text-xs text-zinc-400">{t("embedding.connectToManage")}</p>
            </div>
        );
    }

    return (
        <div className="max-w-2xl space-y-4">
            {/* Service status */}
            <div className="rounded-md border border-zinc-200 bg-modal-surface p-4 dark:border-zinc-700">
                <h2 className="mb-3 text-xs font-medium">{t("embedding.serviceStatus")}</h2>
                <div className="flex items-center gap-2 text-xs">
                    <span className="text-zinc-500">{t("embedding.status")}</span>
                    <span
                        className={cn(
                            "h-2 w-2 rounded-full",
                            serviceRunning ? "bg-[var(--color-accent)]" : "bg-zinc-400",
                        )}
                    />
                    <span className={cn(
                        serviceRunning ? "text-[var(--color-accent)]" : "text-zinc-500"
                    )}>
                        {serviceRunning ? t("embedding.running") : t("embedding.stopped")}
                    </span>
                </div>
                {activeModelId && serviceRunning && (
                    <div className="mt-2 flex items-center gap-2 text-xs">
                        <span className="text-zinc-500">{t("embedding.activeModel")}</span>
                        <span className="font-medium">{activeModelId}</span>
                    </div>
                )}
                {/* Test button — only when service is running and has active model */}
                {serviceRunning && activeModelId && (
                    <div className="mt-3 flex items-center gap-2">
                        <button
                            onClick={handleTest}
                            disabled={testing}
                            className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-[11px] font-medium disabled:opacity-50"
                        >
                            {testing ? (
                                <Loader2 className="h-3 w-3 animate-spin" />
                            ) : (
                                <Zap className="h-3 w-3" />
                            )}
                            {testing ? t("embedding.testing") : t("embedding.test")}
                        </button>
                        {/* Test result inline */}
                        {testResult && (
                            <span className="flex items-center gap-1 text-[11px]">
                                {testResult.success ? (
                                    <>
                                        <CheckCircle2 className="h-3 w-3 text-green-500" />
                                        <span className="text-green-600 dark:text-green-400">
                                            {t("embedding.testPassed")}
                                            {testResult.dimension && ` (${testResult.dimension}d)`}
                                            {testResult.latency_ms != null && ` ${testResult.latency_ms}ms`}
                                        </span>
                                    </>
                                ) : (
                                    <>
                                        <XCircle className="h-3 w-3 text-red-500" />
                                        <span className="text-red-600 dark:text-red-400">
                                            {testResult.error ?? t("embedding.testFailed")}
                                        </span>
                                    </>
                                )}
                            </span>
                        )}
                    </div>
                )}
            </div>

            {/* Error message */}
            {error && (
                <ErrorBox
                    message={error}
                    onClose={() => setError(null)}
                />
            )}

            {/* Migration panel */}
            {migrationResponse && (
                <MigrationPanel
                    migrationResponse={migrationResponse}
                    migrationAgentIds={migrationAgentIds}
                    migrationStarting={migrationStarting}
                    migrationStarted={migrationStarted}
                    migrationProgress={migrationProgress}
                    onToggleAgent={toggleMigrationAgent}
                    onStartMigration={handleStartMigration}
                    onCancel={handleMigrationCancel}
                />
            )}

            {/* Model list */}
            <div className="rounded-md border border-zinc-200 bg-modal-surface p-4 dark:border-zinc-700">
                <div className="mb-3 flex items-center justify-between">
                    <h2 className="inline-flex items-center gap-1.5 text-xs font-medium">
                        <HardDrive className="h-3.5 w-3.5" />
                        {t("embedding.localModels")}
                    </h2>
                    <button
                        onClick={loadModels}
                        disabled={loading}
                        className="text-xs text-zinc-500 hover:text-zinc-700 dark:text-zinc-400 dark:hover:text-zinc-300"
                    >
                        {loading ? t("embedding.loading") : t("embedding.refresh")}
                    </button>
                </div>

                {loading && models.length === 0 ? (
                    <p className="text-xs text-zinc-400">{t("embedding.loading")}</p>
                ) : models.length === 0 ? (
                    <p className="text-xs text-zinc-400">{t("embedding.noModels")}</p>
                ) : (
                    <div className="space-y-2">
                        {models.map((model) => (
                            <ModelCard
                                key={model.id}
                                model={model}
                                isActive={model.id === activeModelId}
                                isDownloading={downloadingIds.has(model.id)}
                                isSelecting={selectingId === model.id}
                                isDeleting={deletingId === model.id}
                                progress={downloadProgress[model.id]}
                                onDownload={handleDownload}
                                onSelect={() => handleSelect(model.id)}
                                onDelete={() => setDeleteConfirm({ modelId: model.id, modelName: model.name })}
                            />
                        ))}
                    </div>
                )}
            </div>

            {/* Cloud Embedding Providers (S1-7) */}
            <div className="rounded-md border border-zinc-200 bg-modal-surface p-4 dark:border-zinc-700">
                <div className="mb-3 flex items-center justify-between">
                    <h2 className="inline-flex items-center gap-1.5 text-xs font-medium">
                        <Cloud className="h-3.5 w-3.5" />
                        {t("embedding.cloudProviders")}
                    </h2>
                    <div className="flex items-center gap-3">
                        <button
                            onClick={() => setCustomDialogOpen(true)}
                            className="inline-flex items-center gap-1 text-xs text-zinc-500 hover:text-zinc-700 dark:text-zinc-400 dark:hover:text-zinc-300"
                        >
                            <Plus className="h-3 w-3" />
                            {t("embedding.addCustom")}
                        </button>
                        <button
                            onClick={loadCloudProviders}
                            disabled={cloudLoading}
                            className="text-xs text-zinc-500 hover:text-zinc-700 dark:text-zinc-400 dark:hover:text-zinc-300"
                        >
                            {cloudLoading ? t("embedding.loading") : t("embedding.refresh")}
                        </button>
                    </div>
                </div>

                {/* Active cloud selection summary */}
                {cloudActive && (
                    <div className="mb-3 flex items-center justify-between rounded border border-zinc-200 bg-zinc-50 px-2 py-1.5 text-[11px] dark:border-zinc-700 dark:bg-zinc-800">
                        <div className="flex items-center gap-2">
                            <span
                                className="rounded px-1.5 py-0.5 text-[10px] font-medium"
                                style={{
                                    backgroundColor:
                                        "color-mix(in srgb, var(--color-accent) 15%, transparent)",
                                    color: "var(--color-accent)",
                                }}
                            >
                                {t("embedding.cloudActive")}
                            </span>
                            <span className="font-medium">
                                {cloudActive.provider_id}/{cloudActive.model_id}
                            </span>
                            <span className="text-zinc-500">· {cloudActive.dimension}d</span>
                            {!cloudActive.has_api_key && (
                                <span className="text-amber-600 dark:text-amber-400">
                                    · {t("embedding.apiKeyMissing")}
                                </span>
                            )}
                        </div>
                    </div>
                )}

                {/* Cloud error inline */}
                {cloudError && (
                    <div className="mb-2 rounded border border-red-200 bg-red-50 px-2 py-1 text-[11px] text-red-700 dark:border-red-800 dark:bg-red-950 dark:text-red-300">
                        {cloudError}
                    </div>
                )}

                {cloudProviders.length === 0 ? (
                    <p className="text-xs text-zinc-400">{t("embedding.cloudNoProviders")}</p>
                ) : (
                    <div className="space-y-3">
                        {cloudProviders.map((provider) => (
                            <CloudProviderCard
                                key={provider.id}
                                provider={provider}
                                active={cloudActive?.provider_id === provider.id ? cloudActive : null}
                                testing={cloudTesting === provider.id}
                                testResult={
                                    // Hide result while testing this card OR while editing
                                    // its key. Otherwise render this provider's own result
                                    // (was previously a single global state — caused
                                    //  one card's failure to bleed into every other card).
                                    cloudTesting === provider.id || keyEditingProvider === provider.id
                                        ? null
                                        : cloudTestResults[provider.id] ?? null
                                }
                                selectingModelId={cloudSelecting?.startsWith(`${provider.id}/`)
                                    ? cloudSelecting.split("/", 2)[1] ?? null
                                    : null}
                                keyEditing={keyEditingProvider === provider.id}
                                keyDraft={keyDraft}
                                keySaving={keySaving}
                                onChangeKeyDraft={setKeyDraft}
                                onStartKeyEdit={() => {
                                    setKeyEditingProvider(provider.id);
                                    setKeyDraft("");
                                    // Clear only this provider's previous test result
                                    setCloudTestResults((prev) => {
                                        if (!(provider.id in prev)) return prev;
                                        const next = { ...prev };
                                        delete next[provider.id];
                                        return next;
                                    });
                                }}
                                onCancelKeyEdit={() => {
                                    setKeyEditingProvider(null);
                                    setKeyDraft("");
                                }}
                                onSubmitKey={() => handleCloudKeySubmit(provider.id)}
                                onDeleteKey={() => handleCloudKeyDelete(provider.id)}
                                onTest={() => handleCloudTest(provider.id)}
                                onSelectModel={(modelId) => handleCloudSelect(provider.id, modelId)}
                            />
                        ))}
                    </div>
                )}
            </div>

            {/* Add custom cloud embedding provider dialog (S2) */}
            <AddCustomEmbeddingProviderDialog
                open={customDialogOpen}
                onClose={() => setCustomDialogOpen(false)}
                onSaved={async () => {
                    setCustomDialogOpen(false);
                    await loadCloudProviders();
                }}
                existingIds={cloudProviders.map((p) => p.id)}
            />

            {/* Dimension mismatch confirmation dialog */}
            {dimensionConfirm && (
                <ConfirmDialog
                    open={true}
                    title={t("embedding.dimensionChange")}
                    message={dimensionConfirm.message}
                    confirmLabel={t("embedding.confirmSwitch")}
                    destructive
                    onConfirm={handleDimensionConfirm}
                    onCancel={() => {
                        setDimensionConfirm(null);
                        setSelectingId(null);
                    }}
                />
            )}

            {/* Delete model confirmation dialog */}
            {deleteConfirm && (
                <ConfirmDialog
                    open={true}
                    title={t("embedding.deleteConfirmTitle")}
                    message={t("embedding.deleteConfirmMessage", { name: deleteConfirm.modelName })}
                    confirmLabel={t("embedding.deleteConfirm")}
                    destructive
                    onConfirm={handleDeleteConfirm}
                    onCancel={() => setDeleteConfirm(null)}
                />
            )}
        </div>
    );
}

/** Migration progress panel — shows agent migration queue and progress */
function MigrationPanel({
    migrationResponse,
    migrationAgentIds,
    migrationStarting,
    migrationStarted,
    migrationProgress,
    onToggleAgent,
    onStartMigration,
    onCancel,
}: {
    migrationResponse: SelectModelMigrationResponse;
    migrationAgentIds: Set<string>;
    migrationStarting: boolean;
    migrationStarted: boolean;
    migrationProgress: Record<string, { progress?: { rebuilt: number; total_scanned: number; errors: number; phase: string; label: string } | null; done: boolean; error?: string | null }>;
    onToggleAgent: (agentId: string) => void;
    onStartMigration: () => void;
    onCancel: () => void;
}) {
    const allDone = migrationResponse.agents
        .filter((a) => migrationAgentIds.has(a.agent_id))
        .every((a) => {
            const p = migrationProgress[a.agent_id];
            return p?.done;
        });

    return (
        <div className="rounded-md border border-amber-200 bg-amber-50 p-4 dark:border-amber-800 dark:bg-amber-900/20">
            <h2 className="mb-2 text-xs font-medium text-amber-800 dark:text-amber-300">
                {migrationStarted
                    ? "Embedding Migration in Progress"
                    : "Embedding Dimension Migration Required"}
            </h2>
            <p className="mb-3 text-[11px] text-amber-700 dark:text-amber-400">
                {migrationResponse.message}
                {` (Old: ${migrationResponse.old_dimension ?? "?"}, New: ${migrationResponse.new_dimension})`}
            </p>

            {/* Agent list */}
            <div className="mb-3 space-y-1.5">
                {migrationResponse.agents.map((agent) => {
                    const isSelected = migrationAgentIds.has(agent.agent_id);
                    const prog = migrationProgress[agent.agent_id];
                    const pct = prog?.progress?.total_scanned
                        ? Math.round((prog.progress.rebuilt / prog.progress.total_scanned) * 100)
                        : 0;
                    const isDone = prog?.done;
                    const hasError = prog?.error;

                    return (
                        <div
                            key={agent.agent_id}
                            className="flex items-center gap-2 rounded border border-amber-200 bg-modal-surface px-3 py-2 text-xs dark:border-amber-700"
                        >
                            {/* Checkbox (only before migration starts) */}
                            {!migrationStarted && (
                                <input
                                    type="checkbox"
                                    checked={isSelected}
                                    disabled={!agent.is_running}
                                    onChange={() => onToggleAgent(agent.agent_id)}
                                    className="h-3.5 w-3.5"
                                />
                            )}

                            {/* Agent name */}
                            <span className="min-w-[100px] truncate font-medium">
                                {agent.name !== agent.agent_id ? agent.name : agent.agent_id}
                            </span>

                            {/* Status badge */}
                            {!agent.is_running ? (
                                <span className="rounded bg-zinc-200 px-1.5 py-0.5 text-[10px] text-zinc-600 dark:bg-zinc-700 dark:text-zinc-400">
                                    Not Running
                                </span>
                            ) : isDone ? (
                                <span className="rounded bg-green-100 px-1.5 py-0.5 text-[10px] text-green-700 dark:bg-green-900/50 dark:text-green-400">
                                    Done ✓
                                </span>
                            ) : hasError ? (
                                <span className="rounded bg-red-100 px-1.5 py-0.5 text-[10px] text-red-700 dark:bg-red-900/50 dark:text-red-400">
                                    Failed ✗
                                </span>
                            ) : migrationStarted && prog ? (
                                <span className="rounded bg-blue-100 px-1.5 py-0.5 text-[10px] text-blue-700 dark:bg-blue-900/50 dark:text-blue-400">
                                    {pct}%
                                </span>
                            ) : (
                                <span className="rounded bg-amber-100 px-1.5 py-0.5 text-[10px] text-amber-700 dark:bg-amber-900/50 dark:text-amber-400">
                                    Pending
                                </span>
                            )}

                            {/* Progress bar */}
                            {migrationStarted && prog && !isDone && !hasError && (
                                <div className="ml-auto flex w-24 items-center gap-1">
                                    <div className="h-1.5 flex-1 overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-700">
                                        <div
                                            className="h-full rounded-full bg-blue-500 transition-all"
                                            style={{ width: `${pct}%` }}
                                        />
                                    </div>
                                    <span className="text-[10px] tabular-nums text-zinc-500">
                                        {prog.progress?.rebuilt ?? 0}/{prog.progress?.total_scanned ?? "?"}
                                    </span>
                                </div>
                            )}
                        </div>
                    );
                })}
            </div>

            {/* Actions */}
            <div className="flex items-center gap-2">
                {!migrationStarted ? (
                    <>
                        <button
                            onClick={onStartMigration}
                            disabled={migrationStarting || migrationAgentIds.size === 0}
                            className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
                        >
                            {migrationStarting ? "Starting..." : "Start Migration"}
                        </button>
                        <button
                            onClick={onCancel}
                            className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium"
                        >
                            Cancel
                        </button>
                    </>
                ) : allDone ? (
                    <button
                        onClick={onCancel}
                        className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium"
                    >
                        Migration Complete — Dismiss
                    </button>
                ) : (
                    <span className="text-xs text-amber-700 dark:text-amber-400">
                        ⏳ Migrating agents... Do not close this panel.
                    </span>
                )}
            </div>
        </div>
    );
}

/** Variant display label */
const VARIANT_LABELS: Record<string, string> = {
    fp32: "FP32",
    fp16: "FP16",
    int8: "INT8",
};

function ModelCard({
    model,
    isActive,
    isDownloading,
    isSelecting,
    isDeleting,
    progress,
    onDownload,
    onSelect,
    onDelete,
}: {
    model: EmbeddingModelWithStatus;
    isActive: boolean;
    isDownloading: boolean;
    isSelecting: boolean;
    isDeleting: boolean;
    progress?: number;
    onDownload: (modelId: string, variant?: string) => void;
    onSelect: () => void;
    onDelete: () => void;
}) {
    const { t } = useTranslation();
    const variants = model.onnx_variants ? Object.keys(model.onnx_variants) : [];
    const hasVariants = variants.length > 1;
    const [selectedVariant, setSelectedVariant] = useState<string>(
        variants.includes("fp16") ? "fp16" : (variants[0] ?? "fp32"),
    );

    const isBusy = isDownloading || isSelecting || isDeleting;

    return (
        <div
            className={cn(
                "rounded-md border p-3 transition-colors",
                isActive
                    ? "border-[var(--color-accent)]/30 bg-[var(--color-accent)]/5 dark:border-[var(--color-accent)]/20 dark:bg-[var(--color-accent)]/5"
                    : "border-zinc-200 bg-modal-surface dark:border-zinc-700",
            )}
        >
            {/* Header: name + badges */}
            <div className="flex items-start justify-between gap-2">
                <div className="min-w-0">
                    <div className="flex items-center gap-2">
                        <span className="text-xs font-semibold">{model.name}</span>
                        {model.recommended && (
                            <span
                                className="rounded px-1.5 py-0.5 text-[10px] font-medium"
                                style={{ backgroundColor: "color-mix(in srgb, var(--color-accent) 15%, transparent)", color: "var(--color-accent)" }}
                            >
                                {t("embedding.recommended")}
                            </span>
                        )}
                        {isActive && (
                            <span
                                className="rounded px-1.5 py-0.5 text-[10px] font-medium"
                                style={{ backgroundColor: "color-mix(in srgb, var(--color-accent) 15%, transparent)", color: "var(--color-accent)" }}
                            >
                                {t("embedding.active")}
                            </span>
                        )}
                    </div>
                    <p className="mt-0.5 text-[10px] text-zinc-500 dark:text-zinc-400">{model.id}</p>
                </div>
                <div className="flex shrink-0 items-center gap-1.5">
                    {/* Variant selector — show when downloading and model has multiple variants */}
                    {hasVariants && (model.status === "not_downloaded" || model.status === "service_not_running" || model.status === "unknown" || model.status === "downloading" || model.status.startsWith("failed")) && (
                        <Dropdown
                            size="small"
                            value={selectedVariant}
                            onChange={setSelectedVariant}
                            disabled={isBusy}
                            options={variants.map((v) => ({
                                value: v,
                                label: VARIANT_LABELS[v] ?? v.toUpperCase(),
                            }))}
                        />
                    )}
                    {/* Download button — show when not downloaded/loaded or unknown */}
                    {(model.status === "not_downloaded" || model.status === "service_not_running" || model.status === "unknown" || model.status === "downloading" || model.status.startsWith("failed")) && (
                        <button
                            onClick={() => onDownload(model.id, hasVariants ? selectedVariant : undefined)}
                            disabled={isBusy || !model.id}
                            className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-[11px] font-medium disabled:opacity-50"
                        >
                            {isDownloading ? (
                                <Loader2 className="h-3 w-3 animate-spin" />
                            ) : (
                                <Download className="h-3 w-3" />
                            )}
                            {isDownloading ? t("embedding.downloading") : t("embedding.download")}
                        </button>
                    )}
                    {/* Select button — show when downloaded but not active */}
                    {!isActive && (model.status === "downloaded" || model.status === "loaded") && (
                        <button
                            onClick={onSelect}
                            disabled={isBusy}
                            className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-[11px] font-medium disabled:opacity-50"
                        >
                            {isSelecting ? (
                                <Loader2 className="h-3 w-3 animate-spin" />
                            ) : (
                                <Check className="h-3 w-3" />
                            )}
                            {isSelecting ? t("embedding.switching") : t("embedding.switchTo")}
                        </button>
                    )}
                    {/* Delete button — show when downloaded and not active */}
                    {!isActive && (model.status === "downloaded" || model.status === "loaded") && (
                        <button
                            onClick={onDelete}
                            disabled={isBusy}
                            className="group/del inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-[11px] font-medium disabled:opacity-50"
                        >
                            {isDeleting ? (
                                <Loader2 className="h-3 w-3 animate-spin" />
                            ) : (
                                <Trash2 className="h-3 w-3 transition-colors group-hover/del:text-[var(--color-accent)]" />
                            )}
                            <span className="transition-colors group-hover/del:text-[var(--color-accent)]">
                                {isDeleting ? t("embedding.deleting") : t("embedding.delete")}
                            </span>
                        </button>
                    )}
                </div>
            </div>

            {/* Download progress bar */}
            {isDownloading && typeof progress === "number" && (
                <div className="mt-2 space-y-1">
                    <div className="h-1.5 w-full overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-700">
                        <div
                            className="h-full rounded-full transition-all duration-300"
                            style={{
                                width: `${Math.max(progress, 2)}%`,
                                backgroundColor: "var(--color-accent)",
                            }}
                        />
                    </div>
                    <p className="text-right text-[10px] text-zinc-500 dark:text-zinc-400">
                        {progress > 0 ? `${progress}%` : t("embedding.connecting")}
                    </p>
                </div>
            )}

            {/* Meta info */}
            <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[10px] text-zinc-500 dark:text-zinc-400">
                <span className="inline-flex items-center gap-1">
                    <Cpu className="h-3 w-3" />
                    {model.dimension}d
                </span>
                <span>{model.size_mb} MB</span>
                <span>{model.max_tokens} tokens</span>
                {model.languages.length > 0 && (
                    <span className="inline-flex items-center gap-1">
                        <Languages className="h-3 w-3" />
                        {model.languages.join(", ")}
                    </span>
                )}
                {hasVariants && (
                    <span className="text-zinc-400">
                        {t("embedding.variants")}: {variants.map((v) => VARIANT_LABELS[v] ?? v.toUpperCase()).join("/")}
                    </span>
                )}
            </div>
        </div>
    );
}

// ── Cloud Provider Card (S1-7) ──────────────────────────────────────────

interface CloudProviderCardProps {
    provider: CloudEmbeddingProvider;
    active: ActiveCloudEmbeddingProvider | null;
    testing: boolean;
    testResult: EmbeddingTestResponse | null;
    selectingModelId: string | null;
    keyEditing: boolean;
    keyDraft: string;
    keySaving: boolean;
    onChangeKeyDraft: (s: string) => void;
    onStartKeyEdit: () => void;
    onCancelKeyEdit: () => void;
    onSubmitKey: () => void;
    onDeleteKey: () => void;
    onTest: () => void;
    onSelectModel: (modelId: string) => void;
}

function CloudProviderCard({
    provider,
    active,
    testing,
    testResult,
    selectingModelId,
    keyEditing,
    keyDraft,
    keySaving,
    onChangeKeyDraft,
    onStartKeyEdit,
    onCancelKeyEdit,
    onSubmitKey,
    onDeleteKey,
    onTest,
    onSelectModel,
}: CloudProviderCardProps) {
    const { t } = useTranslation();
    // Each provider card must reflect its OWN key state, not just the
    // active selection's. Backend returns `has_api_key` per-provider on
    // `GET /api/embedding-providers`; fall back to the active selection
    // (only valid when this card IS the active provider) for safety.
    const hasKey = provider.has_api_key ?? active?.has_api_key ?? false;
    const isActiveProvider = active?.provider_id === provider.id;

    return (
        <div className="rounded-md border border-zinc-200 bg-white p-3 dark:border-zinc-700 dark:bg-zinc-900">
            {/* Header: name + api + key state */}
            <div className="mb-2 flex items-center justify-between gap-2">
                <div className="min-w-0">
                    <div className="flex items-center gap-2">
                        <h3 className="text-xs font-medium">{provider.name}</h3>
                        <span className="text-[10px] text-zinc-400">({provider.id})</span>
                    </div>
                    <p className="mt-0.5 truncate font-mono text-[10px] text-zinc-500 dark:text-zinc-400">
                        {provider.api}
                    </p>
                </div>
                <div className="flex shrink-0 items-center gap-1.5">
                    {!keyEditing ? (
                        hasKey ? (
                            <>
                                <span className="inline-flex items-center gap-1 rounded bg-green-50 px-1.5 py-0.5 text-[10px] font-medium text-green-700 dark:bg-green-950 dark:text-green-300">
                                    <CheckCircle2 className="h-2.5 w-2.5" />
                                    Key
                                </span>
                                <button
                                    onClick={onTest}
                                    disabled={testing}
                                    className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-[11px] font-medium disabled:opacity-50"
                                >
                                    {testing ? (
                                        <Loader2 className="h-3 w-3 animate-spin" />
                                    ) : (
                                        <Zap className="h-3 w-3" />
                                    )}
                                    {testing ? t("embedding.testing") : t("embedding.test")}
                                </button>
                                <button
                                    onClick={onStartKeyEdit}
                                    className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-[11px] font-medium"
                                >
                                    {t("embedding.changeKey")}
                                </button>
                                <button
                                    onClick={onDeleteKey}
                                    className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-[11px] font-medium"
                                >
                                    {t("embedding.deleteKey")}
                                </button>
                            </>
                        ) : (
                            <button
                                onClick={onStartKeyEdit}
                                className="inline-flex items-center gap-1 rounded btn-accent px-2 py-1 text-[11px] font-medium"
                            >
                                <KeyRound className="h-3 w-3" />
                                {t("embedding.setApiKey")}
                            </button>
                        )
                    ) : (
                        <div className="flex items-center gap-1.5">
                            <input
                                type="password"
                                value={keyDraft}
                                onChange={(e) => onChangeKeyDraft(e.target.value)}
                                placeholder={provider.env[0] ?? "API Key"}
                                className="w-44 rounded-md border border-zinc-300 px-2 py-1 text-[11px] dark:border-zinc-600 dark:bg-zinc-800"
                                autoFocus
                                onKeyDown={(e) => {
                                    if (e.key === "Enter") onSubmitKey();
                                    if (e.key === "Escape") onCancelKeyEdit();
                                }}
                            />
                            <button
                                onClick={onSubmitKey}
                                disabled={keySaving || !keyDraft.trim()}
                                className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-[11px] font-medium disabled:opacity-50"
                            >
                                {keySaving ? (
                                    <Loader2 className="h-3 w-3 animate-spin" />
                                ) : (
                                    <Check className="h-3 w-3" />
                                )}
                                {t("embedding.save")}
                            </button>
                            <button
                                onClick={onCancelKeyEdit}
                                disabled={keySaving}
                                className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-[11px] font-medium disabled:opacity-50"
                            >
                                {t("embedding.cancel")}
                            </button>
                        </div>
                    )}
                </div>
            </div>

            {/* Test result inline */}
            {testResult && (
                <div className="mb-2 flex items-center gap-1 text-[11px]">
                    {testResult.success ? (
                        <>
                            <CheckCircle2 className="h-3 w-3 text-green-500" />
                            <span className="text-green-600 dark:text-green-400">
                                {t("embedding.testPassed")}
                                {testResult.dimension && ` (${testResult.dimension}d)`}
                                {testResult.latency_ms != null && ` ${testResult.latency_ms}ms`}
                            </span>
                        </>
                    ) : (
                        <>
                            <XCircle className="h-3 w-3 text-red-500" />
                            <span className="text-red-600 dark:text-red-400">
                                {testResult.error ?? t("embedding.testFailed")}
                            </span>
                        </>
                    )}
                </div>
            )}

            {/* Model list */}
            <div className="space-y-1">
                {Object.values(provider.models).map((m) => {
                    const isActiveModel = isActiveProvider && active?.model_id === m.id;
                    const isSelecting = selectingModelId === m.id;
                    return (
                        <div
                            key={m.id}
                            className="flex items-center justify-between rounded border border-zinc-100 px-2 py-1.5 text-[11px] dark:border-zinc-800"
                        >
                            <div className="flex min-w-0 items-center gap-2">
                                <span className="font-medium">{m.name || m.id}</span>
                                <span className="font-mono text-[10px] text-zinc-500">{m.id}</span>
                                <span className="text-zinc-400">· {m.dimensions}d</span>
                                {m.context_length && (
                                    <span className="text-zinc-400">· {m.context_length} ctx</span>
                                )}
                                {isActiveModel && (
                                    <span
                                        className="rounded px-1.5 py-0.5 text-[10px] font-medium"
                                        style={{
                                            backgroundColor:
                                                "color-mix(in srgb, var(--color-accent) 15%, transparent)",
                                            color: "var(--color-accent)",
                                        }}
                                    >
                                        {t("embedding.active")}
                                    </span>
                                )}
                            </div>
                            {!isActiveModel && (
                                <button
                                    onClick={() => onSelectModel(m.id)}
                                    disabled={isSelecting || !hasKey}
                                    title={!hasKey ? t("embedding.apiKeyRequired") : undefined}
                                    className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-[11px] font-medium disabled:opacity-50"
                                >
                                    {isSelecting ? (
                                        <Loader2 className="h-3 w-3 animate-spin" />
                                    ) : (
                                        <Check className="h-3 w-3" />
                                    )}
                                        {isSelecting ? t("embedding.switching") : t("embedding.switchTo")}
                                    </button>
                                )}
                            </div>
                        );
                    })}
                </div>
        </div>
    );
}

// ── Add custom cloud embedding provider dialog (S2) ─────────────────

interface CustomProviderModelDraft {
    id: string;
    name: string;
    dimensions: string;
    context_length: string;
}

const EMPTY_MODEL_DRAFT: CustomProviderModelDraft = {
    id: "",
    name: "",
    dimensions: "",
    context_length: "",
};

interface AddCustomEmbeddingProviderDialogProps {
    open: boolean;
    onClose: () => void;
    onSaved: () => void | Promise<void>;
    existingIds: string[];
}

function AddCustomEmbeddingProviderDialog({
    open,
    onClose,
    onSaved,
    existingIds,
}: AddCustomEmbeddingProviderDialogProps) {
    const { t } = useTranslation();
    const [name, setName] = useState("");
    const [id, setId] = useState("");
    const [api, setApi] = useState("");
    const [apiKey, setApiKey] = useState("");
    const [models, setModels] = useState<CustomProviderModelDraft[]>([{ ...EMPTY_MODEL_DRAFT }]);
    const [saving, setSaving] = useState(false);
    const [error, setError] = useState<string | null>(null);

    if (!open) return null;

    const slugifyProviderId = (raw: string): string => {
        return (
            "custom-" +
            raw
                .toLowerCase()
                .replace(/[^a-z0-9]+/g, "-")
                .replace(/^-|-$/g, "")
        );
    };

    const handleNameChange = (v: string) => {
        setName(v);
        setId(slugifyProviderId(v));
    };

    const updateModel = (idx: number, patch: Partial<CustomProviderModelDraft>) => {
        setModels((prev) => prev.map((m, i) => (i === idx ? { ...m, ...patch } : m)));
    };

    const addModel = () => {
        setModels((prev) => [...prev, { ...EMPTY_MODEL_DRAFT }]);
    };

    const removeModel = (idx: number) => {
        setModels((prev) => prev.filter((_, i) => i !== idx));
    };

    const reset = () => {
        setName("");
        setId("");
        setApi("");
        setApiKey("");
        setModels([{ ...EMPTY_MODEL_DRAFT }]);
        setError(null);
        setSaving(false);
    };

    const handleClose = () => {
        reset();
        onClose();
    };

    const handleSave = async () => {
        setError(null);
        const trimmedName = name.trim();
        const trimmedId = id.trim();
        const trimmedApi = api.trim();
        if (!trimmedName) {
            setError(t("embedding.customProviderNameRequired"));
            return;
        }
        if (!trimmedId) {
            setError(t("embedding.customProviderIdRequired"));
            return;
        }
        if (existingIds.includes(trimmedId)) {
            setError(t("embedding.duplicateProviderId", { id: trimmedId }));
            return;
        }
        if (!trimmedApi || !(trimmedApi.startsWith("http://") || trimmedApi.startsWith("https://"))) {
            setError(t("embedding.customBaseUrlRequired"));
            return;
        }
        if (models.length === 0) {
            setError(t("embedding.customModelsRequired"));
            return;
        }
        const modelsMap: Record<string, { id: string; name: string; dimensions: number; context_length?: number | null; embedding_modalities?: string[] }> = {};
        for (const m of models) {
            const mid = m.id.trim();
            const mname = m.name.trim();
            const dim = parseInt(m.dimensions.trim(), 10);
            if (!mid) {
                setError(t("embedding.customModelIdRequired"));
                return;
            }
            if (!mid) continue;
            if (modelsMap[mid]) {
                setError(t("embedding.duplicateModelId", { id: mid }));
                return;
            }
            if (!Number.isFinite(dim) || dim <= 0) {
                setError(t("embedding.dimensionMustBePositive", { id: mid }));
                return;
            }
            const ctxStr = m.context_length.trim();
            const ctx = ctxStr ? parseInt(ctxStr, 10) : null;
            modelsMap[mid] = {
                id: mid,
                name: mname || mid,
                dimensions: dim,
                context_length: ctx && Number.isFinite(ctx) && ctx > 0 ? ctx : null,
                embedding_modalities: ["text"],
            };
        }

        setSaving(true);
        try {
            await addCloudEmbeddingProvider({
                id: trimmedId,
                name: trimmedName,
                api: trimmedApi,
                models: modelsMap,
                api_key: apiKey.trim() || undefined,
            });
            await onSaved();
            reset();
        } catch (e: any) {
            setError(e?.message || String(e));
        } finally {
            setSaving(false);
        }
    };

    return (
        <div
            className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay"
            onClick={handleClose}
        >
            <div
                className="w-[520px] max-h-[85vh] overflow-hidden rounded-md bg-modal-surface shadow-xl flex flex-col"
                onClick={(e) => e.stopPropagation()}
            >
                {/* Header */}
                <div className="shrink-0 flex items-center gap-2 px-6 pt-6 pb-3">
                    <h3 className="text-sm font-semibold">
                        {t("embedding.addCustomProvider")}
                    </h3>
                </div>

                {/* Scrollable content */}
                <div className="flex-1 overflow-y-auto px-6 pb-2 space-y-3">
                    {/* Provider Name */}
                    <div>
                        <label className="mb-1 block text-xs text-zinc-500">
                            {t("embedding.customProviderName")}
                        </label>
                        <input
                            type="text"
                            value={name}
                            onChange={(e) => handleNameChange(e.target.value)}
                            placeholder={t("embedding.customProviderNamePlaceholder")}
                            className="w-full rounded-md border border-zinc-200 bg-modal-surface px-3 py-2 text-xs dark:border-zinc-700"
                        />
                    </div>

                    {/* Provider ID */}
                    <div>
                        <label className="mb-1 block text-xs text-zinc-500">
                            {t("embedding.customProviderId")}
                        </label>
                        <input
                            type="text"
                            value={id}
                            onChange={(e) => setId(e.target.value)}
                            placeholder={t("embedding.customProviderIdPlaceholder")}
                            className="w-full rounded-md border border-zinc-200 bg-modal-surface px-3 py-2 text-xs font-mono dark:border-zinc-700"
                        />
                    </div>

                    {/* Base URL */}
                    <div>
                        <label className="mb-1 block text-xs text-zinc-500">
                            {t("embedding.customBaseUrl")}
                        </label>
                        <input
                            type="text"
                            value={api}
                            onChange={(e) => setApi(e.target.value)}
                            placeholder="https://api.example.com/v1"
                            className="w-full rounded-md border border-zinc-200 bg-modal-surface px-3 py-2 text-xs font-mono dark:border-zinc-700"
                        />
                    </div>

                    {/* API Key (optional) */}
                    <div>
                        <label className="mb-1 block text-xs text-zinc-500">
                            {t("embedding.apiKey")}{" "}
                            <span className="text-zinc-400">({t("embedding.optional")})</span>
                        </label>
                        <input
                            type="password"
                            value={apiKey}
                            onChange={(e) => setApiKey(e.target.value)}
                            placeholder="sk-..."
                            className="w-full rounded-md border border-zinc-200 bg-modal-surface px-3 py-2 text-xs dark:border-zinc-700"
                        />
                    </div>

                    {/* Models list */}
                    <div>
                        <div className="mb-1 flex items-center justify-between">
                            <label className="text-xs text-zinc-500">
                                {t("embedding.customModels")}
                            </label>
                            <button
                                type="button"
                                onClick={addModel}
                                className="inline-flex items-center gap-1 text-[11px] text-zinc-500 hover:text-zinc-700 dark:text-zinc-400 dark:hover:text-zinc-200"
                            >
                                <Plus className="h-3 w-3" />
                                {t("embedding.customAddModel")}
                            </button>
                        </div>
                        <div className="space-y-2">
                            {models.map((m, idx) => (
                                <div
                                    key={idx}
                                    className="rounded border border-zinc-200 bg-zinc-50 p-2 dark:border-zinc-700 dark:bg-zinc-800"
                                >
                                    <div className="mb-1 grid grid-cols-2 gap-2">
                                        <input
                                            type="text"
                                            value={m.id}
                                            onChange={(e) =>
                                                updateModel(idx, { id: e.target.value })
                                            }
                                            placeholder={t("embedding.customModelIdPlaceholder")}
                                            className="rounded-md border border-zinc-200 bg-modal-surface px-2 py-1 text-[11px] font-mono dark:border-zinc-600"
                                        />
                                        <input
                                            type="text"
                                            value={m.name}
                                            onChange={(e) =>
                                                updateModel(idx, { name: e.target.value })
                                            }
                                            placeholder={t("embedding.customModelNamePlaceholder")}
                                            className="rounded-md border border-zinc-200 bg-modal-surface px-2 py-1 text-[11px] dark:border-zinc-600"
                                        />
                                    </div>
                                    <div className="flex items-center gap-2">
                                        <input
                                            type="number"
                                            value={m.dimensions}
                                            onChange={(e) =>
                                                updateModel(idx, { dimensions: e.target.value })
                                            }
                                            placeholder={t("embedding.customModelDimensionsPlaceholder")}
                                            className="w-24 rounded-md border border-zinc-200 bg-modal-surface px-2 py-1 text-[11px] dark:border-zinc-600"
                                        />
                                        <input
                                            type="number"
                                            value={m.context_length}
                                            onChange={(e) =>
                                                updateModel(idx, {
                                                    context_length: e.target.value,
                                                })
                                            }
                                            placeholder={t("embedding.customModelContextLengthPlaceholder")}
                                            className="w-28 rounded-md border border-zinc-200 bg-modal-surface px-2 py-1 text-[11px] dark:border-zinc-600"
                                        />
                                        {models.length > 1 && (
                                            <button
                                                type="button"
                                                onClick={() => removeModel(idx)}
                                                className="ml-auto text-zinc-400 hover:text-red-500"
                                                title={t("embedding.customRemoveModel")}
                                            >
                                                <Trash2 className="h-3 w-3" />
                                            </button>
                                        )}
                                    </div>
                                </div>
                            ))}
                        </div>
                    </div>

                    {/* Inline error */}
                    {error && (
                        <div className="rounded border border-red-200 bg-red-50 px-2 py-1.5 text-[11px] text-red-700 dark:border-red-800 dark:bg-red-950 dark:text-red-300">
                            {error}
                        </div>
                    )}
                </div>

                {/* Footer */}
                <div className="shrink-0 flex items-center justify-end gap-2 border-t border-zinc-100 dark:border-zinc-800 px-6 py-4">
                    <button
                        onClick={handleClose}
                        disabled={saving}
                        className="rounded-md px-3 py-[var(--ui-btn-py)] text-xs font-medium text-zinc-600 hover:bg-zinc-100 disabled:opacity-50 dark:text-zinc-400 dark:hover:bg-zinc-700"
                    >
                        {t("embedding.cancel")}
                    </button>
                    <button
                        onClick={handleSave}
                        disabled={saving}
                        className="btn-accent rounded-md px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
                    >
                        {saving ? t("embedding.saving") : t("embedding.save")}
                    </button>
                </div>
            </div>
        </div>
    );
}

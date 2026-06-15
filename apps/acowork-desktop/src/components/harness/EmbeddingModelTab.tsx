import { useState, useEffect, useCallback, useRef } from "react";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useTranslation } from "../../i18n/useTranslation";
import type { EmbeddingModelWithStatus } from "../../lib/types";
import { cn } from "../../lib/utils";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { fetchEmbeddingModels, downloadEmbeddingModel, selectEmbeddingModel, fetchEmbeddingModelStatus, testEmbeddingModel, deleteEmbeddingModel } from "../../lib/gateway-api";
import type { EmbeddingTestResponse } from "../../lib/types";
import { Download, Check, Loader2, Cpu, Languages, Zap, CheckCircle2, XCircle, Trash2 } from "lucide-react";

export function EmbeddingModelTab() {
    const { t } = useTranslation();
    const status = useGatewayStore((s) => s.status);
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
    const [testing, setTesting] = useState(false);
    const [testResult, setTestResult] = useState<EmbeddingTestResponse | null>(null);

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
            const result = await selectEmbeddingModel(modelId, force);
            if (result.status === "dimension_mismatch") {
                setDimensionConfirm({ modelId, message: result.message });
                return;
            }
            await loadModels();
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
            <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
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
                            className="inline-flex items-center gap-1 rounded-md border border-zinc-300 px-2 py-1 text-[11px] font-medium text-zinc-700 hover:bg-zinc-50 disabled:opacity-50 dark:border-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-700"
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
                <div className="rounded-lg border border-red-200 bg-red-50 p-3 text-xs text-red-700 dark:border-red-800 dark:bg-red-900/20 dark:text-red-400">
                    {error}
                </div>
            )}

            {/* Model list */}
            <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
                <div className="mb-3 flex items-center justify-between">
                    <h2 className="text-xs font-medium">{t("embedding.availableModels")}</h2>
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
                "rounded-lg border p-3 transition-colors",
                isActive
                    ? "border-[var(--color-accent)]/30 bg-[var(--color-accent)]/5 dark:border-[var(--color-accent)]/20 dark:bg-[var(--color-accent)]/5"
                    : "border-zinc-200 bg-white dark:border-zinc-700 dark:bg-zinc-800",
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
                        <select
                            value={selectedVariant}
                            onChange={(e) => setSelectedVariant(e.target.value)}
                            disabled={isBusy}
                            className="h-7 appearance-none rounded-md border border-zinc-200 bg-white px-1.5 text-[11px] text-zinc-700 outline-none transition-colors focus:border-[var(--color-accent)] dark:border-zinc-600 dark:bg-zinc-800 dark:text-zinc-300"
                            style={{
                                backgroundImage: `url("data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg' fill='none' viewBox='0 0 20 20'%3e%3cpath stroke='%236b7280' stroke-linecap='round' stroke-linejoin='round' stroke-width='1.5' d='M6 8l4 4 4-4'/%3e%3c/svg%3e")`,
                                backgroundPosition: 'right 0.25rem center',
                                backgroundRepeat: 'no-repeat',
                                backgroundSize: '1.2em 1.2em',
                                paddingRight: '1.25rem',
                            }}
                        >
                            {variants.map((v) => (
                                <option key={v} value={v}>
                                    {VARIANT_LABELS[v] ?? v.toUpperCase()}
                                </option>
                            ))}
                        </select>
                    )}
                    {/* Download button — show when not downloaded/loaded or unknown */}
                    {(model.status === "not_downloaded" || model.status === "service_not_running" || model.status === "unknown" || model.status === "downloading" || model.status.startsWith("failed")) && (
                        <button
                            onClick={() => onDownload(model.id, hasVariants ? selectedVariant : undefined)}
                            disabled={isBusy || !model.id}
                            className="inline-flex items-center gap-1 rounded-md border border-zinc-300 px-2 py-1 text-[11px] font-medium text-zinc-700 hover:bg-zinc-50 disabled:opacity-50 dark:border-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-700"
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
                            className="inline-flex items-center gap-1 rounded-md btn-solid px-2 py-1 text-[11px] font-medium disabled:opacity-50"
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
                            className="group/del inline-flex items-center gap-1 rounded-md border border-zinc-300 px-2 py-1 text-[11px] font-medium text-zinc-700 hover:bg-zinc-50 disabled:opacity-50 dark:border-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-700"
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

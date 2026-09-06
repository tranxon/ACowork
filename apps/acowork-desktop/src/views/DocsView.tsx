/**
 * DocsView — doc 视图壳（plan D2-1，替换 AppLayout docs TODO）。
 *
 * 布局：左侧 DocTreeSidebar（260px）+ 右侧列（ReviewQueue 顶条 + DocEditor）。
 * 职责：
 * - 进入视图：健康检查 + 30s 轮询；离线 → 右侧整体离线面板（重试按钮），
 *   sidebar 保留（可浏览已缓存树）；恢复后自动回到编辑器。
 * - 首次健康检查成功后加载根树（sidebar 内部做，这里只负责 health 编排）。
 */

import { useEffect } from "react";
import { BookOpen, Loader2, RefreshCw } from "lucide-react";
import { useTranslation } from "../i18n/useTranslation";
import { useDocHealthStore } from "../stores/doc/healthStore";
import { useDocTreeStore } from "../stores/doc/treeStore";
import { useDocRequestStore } from "../stores/doc/requestStore";
import { DocTreeSidebar } from "./doc/DocTreeSidebar";
import { ReviewQueue } from "./doc/ReviewQueue";
import { DocEditor } from "./doc/DocEditor";
import { DOC_ROOT_DIR_ID } from "../lib/doc-types";

export function DocsView() {
  const { t } = useTranslation();
  const healthy = useDocHealthStore((s) => s.healthy);
  const checking = useDocHealthStore((s) => s.checking);
  const check = useDocHealthStore((s) => s.check);
  const loadDir = useDocTreeStore((s) => s.loadDir);
  const loadPending = useDocRequestStore((s) => s.loadPending);

  // 进入视图：健康检查；检查成功（或已成功）时确保根树加载
  useEffect(() => {
    void check().then((ok) => {
      if (ok) {
        void loadDir(DOC_ROOT_DIR_ID);
        void loadPending();
      }
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 30s 健康轮询；离线 → 恢复在线时重载根树 + pending
  useEffect(() => {
    const timer = setInterval(() => {
      void check().then((ok) => {
        if (ok) {
          void loadDir(DOC_ROOT_DIR_ID);
          void loadPending();
        }
      });
    }, 30_000);
    return () => clearInterval(timer);
  }, [check, loadDir, loadPending]);

  const offline = healthy === false;

  return (
    <div className="flex h-full overflow-hidden rounded-xl bg-chat-area">
      {/* 左侧目录树（离线也可浏览缓存） */}
      <DocTreeSidebar />

      {/* 右侧列 */}
      <div className="flex h-full min-w-0 flex-1 flex-col">
        {offline ? (
          <OfflinePanel
            checking={checking}
            onRetry={() => void check()}
            message={t("doc.offlineTitle")}
            hint={t("doc.offlineHint")}
          />
        ) : (
          <>
            <ReviewQueue />
            <div className="min-h-0 flex-1">
              <DocEditor />
            </div>
          </>
        )}
      </div>
    </div>
  );
}

function OfflinePanel({
  checking,
  onRetry,
  message,
  hint,
}: {
  checking: boolean;
  onRetry: () => void;
  message: string;
  hint: string;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex h-full flex-col items-center justify-center gap-3 p-8 text-center">
      <BookOpen className="h-10 w-10 text-zinc-300 dark:text-zinc-600" aria-hidden />
      <div>
        <p className="text-sm font-medium text-zinc-600 dark:text-zinc-300">{message}</p>
        <p className="mt-1 max-w-sm text-xs leading-relaxed text-zinc-400">{hint}</p>
      </div>
      <button
        type="button"
        onClick={onRetry}
        disabled={checking}
        className="inline-flex items-center gap-1.5 rounded-md bg-[var(--color-accent)] px-3 py-1.5 text-xs font-medium text-white hover:opacity-90 disabled:opacity-60"
      >
        {checking ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : <RefreshCw className="h-3.5 w-3.5" aria-hidden />}
        {t("common.retry")}
      </button>
    </div>
  );
}

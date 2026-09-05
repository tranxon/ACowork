/**
 * DocTreeSidebar — doc 视图左侧目录树（设计 §7）。
 *
 * - 根 = 文档库根（root 固定，不可改名/删除）；目录可展开/折叠（懒加载）。
 * - 行内操作：目录 → 新建子目录/新建文档/重命名/删除；文档 → 重命名/删除。
 * - 删除进回收站（ConfirmDialog 说明可恢复）；回收站入口在底部。
 * - 新建/重命名用 inline input（Enter 确认 / Esc 取消 / blur 提交）。
 *
 * a11y：树形结构 role="tree"，行可聚焦；方向键展开/折叠在 D2-7 细化。
 */

import { useEffect, useRef, useState } from "react";
import {
  BookOpen,
  ChevronRight,
  FilePlus2,
  FileText,
  Folder,
  FolderPlus,
  FolderOpen,
  Loader2,
  Pencil,
  Trash2,
} from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useDocTreeStore } from "../../stores/doc/treeStore";
import { useDocEditorStore } from "../../stores/doc/editorStore";
import { useDocHealthStore } from "../../stores/doc/healthStore";
import { useToast } from "../../components/common/ToastProvider";
import { ConfirmDialog } from "../../components/common/ConfirmDialog";
import { cn } from "../../lib/utils";
import { DOC_ROOT_DIR_ID } from "../../lib/doc-types";
import type { DirMeta, DocMeta } from "../../lib/doc-types";
import { TrashDialog } from "./TrashDialog";

/** inline 编辑态：新建或重命名 */
type InlineEdit =
  | { mode: "create"; kind: "dir" | "doc"; parentDirId: string }
  | { mode: "rename"; kind: "dir" | "doc"; id: string; parentDirId: string; value: string }
  | null;

/** 待确认删除目标（目录级联删除需强调） */
type DeleteTarget =
  | { kind: "doc"; doc: DocMeta; parentDirId: string }
  | { kind: "dir"; dir: DirMeta; parentDirId: string }
  | null;

export function DocTreeSidebar() {
  const { t } = useTranslation();
  const toast = useToast();
  const healthy = useDocHealthStore((s) => s.healthy);
  const rootReady = useDocTreeStore((s) => s.rootReady);
  const nodes = useDocTreeStore((s) => s.nodes);
  const expanded = useDocTreeStore((s) => s.expanded);
  const loadingDirs = useDocTreeStore((s) => s.loadingDirs);
  const selectedDocId = useDocEditorStore((s) => s.doc?.meta.doc_id ?? null);
  const treeError = useDocTreeStore((s) => s.error);
  const clearTreeError = useDocTreeStore((s) => s.clearError);

  const [editing, setEditing] = useState<InlineEdit>(null);
  const [deleteTarget, setDeleteTarget] = useState<DeleteTarget>(null);
  const [trashOpen, setTrashOpen] = useState(false);

  // 首次挂载：加载根（树根不自动展开，懒加载第一层）
  const loadDir = useDocTreeStore((s) => s.loadDir);
  useEffect(() => {
    if (!rootReady && healthy !== false) {
      void loadDir(DOC_ROOT_DIR_ID);
    }
  }, [rootReady, healthy, loadDir]);

  // store 层操作错误 → toast（错误已在 store 记录并 clear）
  useEffect(() => {
    if (!treeError) return;
    toast.addToast({ type: "error", message: treeError });
    clearTreeError();
  }, [treeError, toast, clearTreeError]);

  const handleCreate = (kind: "dir" | "doc", parentDirId: string) => {
    if (!healthy) return;
    setEditing({ mode: "create", kind, parentDirId });
  };
  const handleRename = (kind: "dir" | "doc", id: string, parentDirId: string, value: string) => {
    if (!healthy) return;
    setEditing({ mode: "rename", kind, id, parentDirId, value });
  };
  const requestDelete = (target: DeleteTarget) => {
    if (!healthy) return;
    setDeleteTarget(target);
  };

  return (
    <aside
      className="flex h-full w-60 shrink-0 flex-col border-r border-zinc-200 bg-surface text-xs dark:border-zinc-800 dark:bg-zinc-900/60"
      aria-label={t("doc.sidebarLabel")}
    >
      {/* ── 头部：标题 + 新建按钮 ───────────────────────────── */}
      <div className="flex items-center gap-1 border-b border-zinc-200 px-2 py-1.5 dark:border-zinc-800">
        <BookOpen className="mr-1 h-3.5 w-3.5 text-zinc-500" aria-hidden />
        <span className="flex-1 truncate font-medium text-zinc-700 dark:text-zinc-200">
          {t("doc.title")}
        </span>
        <IconBtn
          label={t("doc.newDir")}
          disabled={healthy === false}
          onClick={() => handleCreate("dir", DOC_ROOT_DIR_ID)}
          title={t("doc.newDir")}
        >
          <FolderPlus className="h-3.5 w-3.5" />
        </IconBtn>
        <IconBtn
          label={t("doc.newDoc")}
          disabled={healthy === false}
          onClick={() => handleCreate("doc", DOC_ROOT_DIR_ID)}
          title={t("doc.newDoc")}
        >
          <FilePlus2 className="h-3.5 w-3.5" />
        </IconBtn>
      </div>

      {/* ── 树 ─────────────────────────────────────────────── */}
      <div className="min-h-0 flex-1 overflow-y-auto px-1 py-1" role="tree" aria-label={t("doc.treeLabel")}>
        <DirContents
          dirId={DOC_ROOT_DIR_ID}
          depth={0}
          nodes={nodes}
          expanded={expanded}
          loadingDirs={loadingDirs}
          selectedDocId={selectedDocId}
          editing={editing}
          onEditChange={setEditing}
          onRequestDelete={requestDelete}
          onCreate={handleCreate}
          onRename={handleRename}
          onLoadDir={loadDir}
        />
      </div>

      {/* ── 底部：回收站 + 离线提示 ─────────────────────────── */}
      <div className="border-t border-zinc-200 p-1 dark:border-zinc-800">
        {healthy === false && (
          <div className="mb-1 rounded-md bg-amber-50 px-2 py-1 text-[11px] text-amber-700 dark:bg-amber-900/30 dark:text-amber-300">
            {t("doc.offlineHint")}
          </div>
        )}
        <button
          type="button"
          onClick={() => setTrashOpen(true)}
          disabled={healthy === false}
          className="flex w-full items-center gap-1.5 rounded-md px-2 py-1 text-zinc-500 transition-colors hover:bg-zinc-100 hover:text-zinc-700 disabled:opacity-40 dark:hover:bg-zinc-800 dark:hover:text-zinc-200"
        >
          <Trash2 className="h-3.5 w-3.5" aria-hidden />
          <span>{t("doc.trash")}</span>
        </button>
      </div>

      {/* ── 删除确认 ────────────────────────────────────────── */}
      <ConfirmDialog
        open={deleteTarget !== null}
        title={deleteTarget?.kind === "dir" ? t("doc.deleteDirTitle") : t("doc.deleteDocTitle")}
        message={
          deleteTarget?.kind === "dir"
            ? t("doc.deleteDirMsg", { name: deleteTarget.dir.name })
            : deleteTarget?.kind === "doc"
              ? t("doc.deleteDocMsg", { name: deleteTarget.doc.name })
              : ""
        }
        confirmLabel={t("common.delete")}
        destructive
        onCancel={() => setDeleteTarget(null)}
        onConfirm={async () => {
          const target = deleteTarget;
          setDeleteTarget(null);
          if (!target) return;
          if (target.kind === "doc") {
            const ok = await useDocTreeStore
              .getState()
              .deleteDoc(target.doc.doc_id, target.parentDirId);
            if (ok) toast.addToast({ type: "success", message: t("doc.deletedToTrash", { name: target.doc.name }) });
          } else {
            const ok = await useDocTreeStore
              .getState()
              .deleteDir(target.dir.dir_id, target.parentDirId);
            if (ok) toast.addToast({ type: "success", message: t("doc.deletedToTrash", { name: target.dir.name }) });
          }
        }}
      />

      <TrashDialog
        open={trashOpen}
        onClose={() => setTrashOpen(false)}
        disabled={healthy === false}
      />
    </aside>
  );
}

/** 行尾小图标按钮（hover 时出现；键盘聚焦也可见） */
function IconBtn({
  label,
  onClick,
  disabled,
  title,
  children,
}: {
  label: string;
  onClick: () => void;
  disabled?: boolean;
  title?: string;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      aria-label={label}
      title={title ?? label}
      disabled={disabled}
      onClick={(e) => {
        e.stopPropagation();
        onClick();
      }}
      className="rounded p-1 text-zinc-400 transition-colors hover:bg-zinc-200 hover:text-zinc-700 focus-visible:bg-zinc-200 focus-visible:text-zinc-700 disabled:opacity-30 dark:hover:bg-zinc-700 dark:hover:text-zinc-100 dark:focus-visible:bg-zinc-700"
    >
      {children}
    </button>
  );
}

/** 某个目录的内容（dirs + files）。dirId = root 时渲染为顶层列表 */
function DirContents({
  dirId,
  depth,
  nodes,
  expanded,
  loadingDirs,
  selectedDocId,
  editing,
  onEditChange,
  onRequestDelete,
  onCreate,
  onRename,
  onLoadDir,
}: {
  dirId: string;
  depth: number;
  nodes: Record<string, import("../../lib/doc-types").DocTreeNode>;
  expanded: Record<string, boolean>;
  loadingDirs: Record<string, boolean>;
  selectedDocId: string | null;
  editing: InlineEdit;
  onEditChange: (e: InlineEdit) => void;
  onRequestDelete: (t: DeleteTarget) => void;
  onCreate: (kind: "dir" | "doc", parentDirId: string) => void;
  onRename: (kind: "dir" | "doc", id: string, parentDirId: string, value: string) => void;
  onLoadDir: (dirId: string, opts?: { force?: boolean }) => Promise<boolean>;
}) {
  const node = nodes[dirId];
  const loading = loadingDirs[dirId];

  if (!node) {
    if (loading) {
      return (
        <div className="flex items-center gap-1 px-2 py-1 text-zinc-400" style={{ paddingLeft: depth * 14 + 8 }}>
          <Loader2 className="h-3 w-3 animate-spin" aria-hidden />
          <span className="text-[11px]">…</span>
        </div>
      );
    }
    return null;
  }

  const isEmpty = node.dirs.length === 0 && node.files.length === 0;
  const creatingHere = editing?.mode === "create" && editing.parentDirId === dirId;

  return (
    <div>
      {node.dirs.map((dir) => (
        <DirRow
          key={dir.dir_id}
          dir={dir}
          parentDirId={dirId}
          depth={depth}
          nodes={nodes}
          expanded={expanded}
          loadingDirs={loadingDirs}
          selectedDocId={selectedDocId}
          editing={editing}
          onEditChange={onEditChange}
          onRequestDelete={onRequestDelete}
          onCreate={onCreate}
          onRename={onRename}
          onLoadDir={onLoadDir}
        />
      ))}
      {node.files.map((file) => (
        <DocRow
          key={file.doc_id}
          doc={file}
          parentDirId={dirId}
          depth={depth}
          selectedDocId={selectedDocId}
          editing={editing}
          onEditChange={onEditChange}
          onRequestDelete={onRequestDelete}
          onRename={onRename}
        />
      ))}
      {isEmpty && dirId === DOC_ROOT_DIR_ID && <div className="h-2" />}
      {creatingHere && (
        <CreateRow
          kind={editing.kind}
          depth={dirId === DOC_ROOT_DIR_ID ? 0 : depth + 1}
          parentDirId={dirId}
          onDone={() => onEditChange(null)}
        />
      )}
    </div>
  );
}

/** 目录行 */
function DirRow({
  dir,
  parentDirId,
  depth,
  nodes,
  expanded,
  loadingDirs,
  selectedDocId,
  editing,
  onEditChange,
  onRequestDelete,
  onCreate,
  onRename,
  onLoadDir,
}: {
  dir: DirMeta;
  parentDirId: string;
  depth: number;
  nodes: Record<string, import("../../lib/doc-types").DocTreeNode>;
  expanded: Record<string, boolean>;
  loadingDirs: Record<string, boolean>;
  selectedDocId: string | null;
  editing: InlineEdit;
  onEditChange: (e: InlineEdit) => void;
  onRequestDelete: (t: DeleteTarget) => void;
  onCreate: (kind: "dir" | "doc", parentDirId: string) => void;
  onRename: (kind: "dir" | "doc", id: string, parentDirId: string, value: string) => void;
  onLoadDir: (dirId: string, opts?: { force?: boolean }) => Promise<boolean>;
}) {
  const { t } = useTranslation();
  const isOpen = !!expanded[dir.dir_id];
  const isLoading = !!loadingDirs[dir.dir_id];
  const childCount = nodes[dir.dir_id]?.files.length ?? 0;

  // inline 重命名当前行
  if (editing?.mode === "rename" && editing.kind === "dir" && editing.id === dir.dir_id) {
    return (
      <RenameRow
        initial={dir.name}
        depth={depth}
        onSubmit={async (name) => {
          const ok = await useDocTreeStore.getState().renameDir(dir.dir_id, parentDirId, name);
          onEditChange(null);
          return ok;
        }}
        onCancel={() => onEditChange(null)}
      />
    );
  }

  const childCreating = editing?.mode === "create" && editing.parentDirId === dir.dir_id;

  return (
    <div role="treeitem" aria-expanded={isOpen} aria-selected={false}>
      <div
        className={cn(
          "group flex cursor-pointer items-center gap-0.5 rounded-md py-1 pr-1 text-zinc-600 hover:bg-zinc-100 dark:text-zinc-300 dark:hover:bg-zinc-800",
        )}
        style={{ paddingLeft: depth * 14 + 4 }}
      >
        <button
          type="button"
          aria-label={isOpen ? t("doc.collapse") : t("doc.expand")}
          onClick={() => void toggle()}
          onKeyDown={(e) => {
            // 键盘导航：→ 展开 / ← 折叠
            if (e.key === "ArrowRight" && !isOpen) {
              e.preventDefault();
              void toggle();
            } else if (e.key === "ArrowLeft" && isOpen) {
              e.preventDefault();
              void toggle();
            }
          }}
          className="flex h-4 w-4 shrink-0 items-center justify-center rounded text-zinc-400 hover:text-zinc-700 dark:hover:text-zinc-100"
        >
          {isLoading ? (
            <Loader2 className="h-3 w-3 animate-spin" aria-hidden />
          ) : (
            <ChevronRight
              className={cn("h-3.5 w-3.5 transition-transform", isOpen && "rotate-90")}
              aria-hidden
            />
          )}
        </button>
        <span
          className="flex min-w-0 flex-1 items-center gap-1.5 py-0.5 select-none"
          onClick={() => void toggle()}
        >
          {isOpen ? (
            <FolderOpen className="h-3.5 w-3.5 shrink-0 text-sky-500" aria-hidden />
          ) : (
            <Folder className="h-3.5 w-3.5 shrink-0 text-sky-500" aria-hidden />
          )}
          <span className="truncate">{dir.name}</span>
          {isOpen && childCount > 0 && (
            <span className="ml-0.5 text-[10px] text-zinc-300 dark:text-zinc-600">{childCount}</span>
          )}
        </span>
        <span className="hidden shrink-0 items-center gap-0 group-hover:flex group-focus-within:flex">
          <IconBtn label={t("doc.newDoc")} onClick={() => onCreate("doc", dir.dir_id)}>
            <FilePlus2 className="h-3 w-3" />
          </IconBtn>
          <IconBtn label={t("doc.newDir")} onClick={() => onCreate("dir", dir.dir_id)}>
            <FolderPlus className="h-3 w-3" />
          </IconBtn>
          <IconBtn
            label={t("doc.rename")}
            onClick={() => onRename("dir", dir.dir_id, parentDirId, dir.name)}
          >
            <Pencil className="h-3 w-3" />
          </IconBtn>
          <IconBtn
            label={t("doc.delete")}
            onClick={() => onRequestDelete({ kind: "dir", dir, parentDirId })}
          >
            <Trash2 className="h-3 w-3" />
          </IconBtn>
        </span>
      </div>

      {isOpen && (
        <DirContents
          dirId={dir.dir_id}
          depth={depth + 1}
          nodes={nodes}
          expanded={expanded}
          loadingDirs={loadingDirs}
          selectedDocId={selectedDocId}
          editing={editing}
          onEditChange={onEditChange}
          onRequestDelete={onRequestDelete}
          onCreate={onCreate}
          onRename={onRename}
          onLoadDir={onLoadDir}
        />
      )}
      {childCreating && (
        <CreateRow
          kind={editing.kind}
          depth={depth + 1}
          parentDirId={dir.dir_id}
          onDone={() => onEditChange(null)}
        />
      )}
    </div>
  );

  async function toggle() {
    if (isOpen) {
      useDocTreeStore.getState().collapseDir(dir.dir_id);
    } else {
      await useDocTreeStore.getState().expandDir(dir.dir_id);
    }
  }
}

/** 文档行 */
function DocRow({
  doc,
  parentDirId,
  depth,
  selectedDocId,
  editing,
  onEditChange,
  onRequestDelete,
  onRename,
}: {
  doc: DocMeta;
  parentDirId: string;
  depth: number;
  selectedDocId: string | null;
  editing: InlineEdit;
  onEditChange: (e: InlineEdit) => void;
  onRequestDelete: (t: DeleteTarget) => void;
  onRename: (kind: "dir" | "doc", id: string, parentDirId: string, value: string) => void;
}) {
  const { t } = useTranslation();
  const requestOpen = useDocEditorStore((s) => s.requestOpen);
  const selected = selectedDocId === doc.doc_id;

  if (editing?.mode === "rename" && editing.kind === "doc" && editing.id === doc.doc_id) {
    return (
      <RenameRow
        initial={doc.name}
        depth={depth}
        onSubmit={async (name) => {
          const ok = await useDocTreeStore
            .getState()
            .renameDoc(doc.doc_id, parentDirId, doc.version, name);
          // 改名后名称变了，编辑器打开的最新元数据也需刷新（重拉）
          if (ok && useDocEditorStore.getState().doc?.meta.doc_id === doc.doc_id) {
            await useDocEditorStore.getState().reload();
          }
          onEditChange(null);
          return ok;
        }}
        onCancel={() => onEditChange(null)}
      />
    );
  }

  return (
    <div role="treeitem" aria-selected={selected}>
      <div
        className={cn(
          "group flex items-center gap-1 rounded-md py-1 pr-1 text-zinc-600 hover:bg-zinc-100 dark:text-zinc-300 dark:hover:bg-zinc-800",
          selected &&
            "bg-[var(--color-accent)]/10 text-[var(--color-accent)] hover:bg-[var(--color-accent)]/15 dark:bg-[var(--color-accent)]/20",
        )}
        style={{ paddingLeft: depth * 14 + 20 }}
      >
        <button
          type="button"
          onClick={() => {
            void requestOpen(doc.doc_id);
          }}
          className={cn(
            "flex min-w-0 flex-1 items-center gap-1.5 py-0.5 text-left select-none",
            selected ? "font-medium" : "",
          )}
        >
          <FileText className="h-3.5 w-3.5 shrink-0 text-zinc-400" aria-hidden />
          <span className="truncate">{doc.name}</span>
        </button>
        <span className="hidden shrink-0 items-center group-hover:flex group-focus-within:flex">
          <IconBtn
            label={t("doc.rename")}
            onClick={() => onRename("doc", doc.doc_id, parentDirId, doc.name)}
          >
            <Pencil className="h-3 w-3" />
          </IconBtn>
          <IconBtn
            label={t("doc.delete")}
            onClick={() => onRequestDelete({ kind: "doc", doc, parentDirId })}
          >
            <Trash2 className="h-3 w-3" />
          </IconBtn>
        </span>
      </div>
    </div>
  );
}

/** inline 新建行（Enter 提交 / Esc 取消） */
function CreateRow({
  kind,
  depth,
  parentDirId,
  onDone,
}: {
  kind: "dir" | "doc";
  depth: number;
  parentDirId: string;
  onDone: () => void;
}) {
  const { t } = useTranslation();
  const [value, setValue] = useState("");

  const submit = async () => {
    const name = value.trim();
    if (!name) {
      onDone();
      return;
    }
    if (kind === "dir") {
      await useDocTreeStore.getState().createDir(parentDirId, name);
    } else {
      await useDocTreeStore.getState().createDoc(parentDirId, name);
    }
    onDone();
  };

  return (
    <div style={{ paddingLeft: depth * 14 + 20 }} className="py-0.5 pr-1">
      <input
        autoFocus
        value={value}
        placeholder={kind === "dir" ? t("doc.dirNamePlaceholder") : t("doc.docNamePlaceholder")}
        onChange={(e) => setValue(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") void submit();
          else if (e.key === "Escape") onDone();
        }}
        onBlur={() => void submit()}
        className="w-full rounded border border-[var(--color-accent)] bg-input px-1.5 py-0.5 text-xs outline-none"
        aria-label={kind === "dir" ? t("doc.newDir") : t("doc.newDoc")}
      />
    </div>
  );
}

/** inline 重命名行 */
function RenameRow({
  initial,
  depth,
  onSubmit,
  onCancel,
}: {
  initial: string;
  depth: number;
  onSubmit: (name: string) => Promise<boolean>;
  onCancel: () => void;
}) {
  const { t } = useTranslation();
  const [value, setValue] = useState(initial);
  const inputRef = useRef<HTMLInputElement>(null);

  // 全选现有名字，方便直接替换
  useEffect(() => {
    inputRef.current?.focus();
    inputRef.current?.select();
  }, []);

  const submit = async () => {
    const name = value.trim();
    if (!name) {
      onCancel();
      return;
    }
    const ok = await onSubmit(name);
    if (!ok) onCancel();
  };

  return (
    <div style={{ paddingLeft: depth * 14 + 20 }} className="py-0.5 pr-1">
      <input
        ref={inputRef}
        value={value}
        onChange={(e) => setValue(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") void submit();
          else if (e.key === "Escape") onCancel();
        }}
        onBlur={() => void submit()}
        className="w-full rounded border border-[var(--color-accent)] bg-input px-1.5 py-0.5 text-xs outline-none"
        aria-label={t("doc.rename")}
      />
    </div>
  );
}

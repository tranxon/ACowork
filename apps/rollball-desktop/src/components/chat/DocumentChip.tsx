import { FileText, FileSpreadsheet, Table, X, Loader, Check, AlertCircle } from "lucide-react";
import { cn } from "../../lib/utils";

/** Status of a document upload/persistence */
export type DocumentChipStatus = "uploading" | "success" | "error";

/** Props for the DocumentChip component */
export interface DocumentChipProps {
  /** Display filename */
  filename: string;
  /** Document format: pdf, docx, pptx, xlsx */
  format: string;
  /** File size in bytes (optional) */
  size?: number;
  /** Upload/persistence status */
  status?: DocumentChipStatus;
  /** Callback to remove the chip (when provided, a remove button is shown) */
  onRemove?: () => void;
  /** Additional CSS classes */
  className?: string;
}

/** Map format to display icon */
function formatIcon(format: string) {
  switch (format) {
    case "pdf":
      return <FileText className="h-4 w-4 shrink-0 text-red-500" />;
    case "docx":
    case "pptx":
      return <FileSpreadsheet className="h-4 w-4 shrink-0 text-blue-500" />;
    case "xlsx":
      return <Table className="h-4 w-4 shrink-0 text-green-600" />;
    default:
      return <FileText className="h-4 w-4 shrink-0 text-zinc-400" />;
  }
}

/** Format bytes to human-readable string */
function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export function DocumentChip({
  filename,
  format,
  size,
  status,
  onRemove,
  className,
}: DocumentChipProps) {
  return (
    <div
      className={cn(
        "inline-flex items-center gap-1.5 rounded-md border px-2.5 py-1 text-xs",
        "border-zinc-200 bg-zinc-50 text-zinc-700",
        "dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-300",
        className,
      )}
    >
      {/* Format icon */}
      {formatIcon(format)}

      {/* Filename + size */}
      <span className="max-w-[200px] truncate font-medium">{filename}</span>
      {size != null && (
        <span className="text-zinc-400 dark:text-zinc-500">
          {formatSize(size)}
        </span>
      )}

      {/* Status indicator */}
      {status === "uploading" && (
        <Loader className="h-3 w-3 shrink-0 animate-spin text-zinc-400" />
      )}
      {status === "success" && (
        <Check className="h-3 w-3 shrink-0 text-green-500" />
      )}
      {status === "error" && (
        <AlertCircle className="h-3 w-3 shrink-0 text-red-500" />
      )}

      {/* Remove button (only when onRemove is provided) */}
      {onRemove && (
        <button
          type="button"
          className="ml-0.5 rounded p-0.5 text-zinc-400 hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
          onClick={(e) => {
            e.stopPropagation();
            onRemove();
          }}
          aria-label={`Remove ${filename}`}
        >
          <X className="h-3 w-3" />
        </button>
      )}
    </div>
  );
}

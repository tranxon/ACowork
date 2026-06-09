import React from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { AlertTriangle, RefreshCw } from "lucide-react";

interface ErrorBoundaryProps {
  children: React.ReactNode;
}

interface ErrorBoundaryState {
  hasError: boolean;
  error: Error | null;
  autoRetried: boolean;
}

// ── Error display component (function component to use hooks) ─────────────

interface ErrorDisplayProps {
  error: Error | null;
  autoRetried: boolean;
  onRetry: () => void;
  onRefresh: () => void;
}

function ErrorDisplay({ error, autoRetried, onRetry, onRefresh }: ErrorDisplayProps) {
  const { t } = useTranslation();

  return (
    <div className="flex h-screen w-screen items-center justify-center bg-zinc-50 dark:bg-zinc-900">
      <div className="mx-auto max-w-md text-center">
        <AlertTriangle className="mx-auto h-12 w-12 text-amber-500" />
        <h2 className="mt-4 text-lg font-semibold text-zinc-900 dark:text-zinc-100">
          {t("errorBoundary.title")}
        </h2>
        <p className="mt-2 text-sm text-zinc-500 dark:text-zinc-400">
          {t("errorBoundary.description")}
        </p>
        {error && (
          <p className="mt-2 max-h-24 overflow-auto rounded bg-zinc-100 p-2 text-xs text-zinc-500 dark:bg-zinc-800 dark:text-zinc-400 select-text">
            {error.message}
          </p>
        )}
        {autoRetried && (
          <p className="mt-3 text-xs text-zinc-400 dark:text-zinc-500">
            {t("errorBoundary.autoRecovering")}
          </p>
        )}
        {!autoRetried && (
          <div className="mt-6 flex items-center justify-center gap-3">
            <button
              onClick={onRetry}
              className="flex items-center gap-2 rounded-lg bg-zinc-200 px-4 py-2 text-sm font-medium text-zinc-700 hover:bg-zinc-300 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
            >
              <RefreshCw className="h-4 w-4" />
              {t("errorBoundary.retry")}
            </button>
            <button
              onClick={onRefresh}
              className="flex items-center gap-2 rounded-lg px-4 py-2 text-sm font-medium text-white"
              style={{ backgroundColor: "var(--color-accent)" }}
              onMouseEnter={(e) => { e.currentTarget.style.filter = "brightness(0.85)"; }}
              onMouseLeave={(e) => { e.currentTarget.style.filter = ""; }}
            >
              <RefreshCw className="h-4 w-4" />
              {t("errorBoundary.refreshPage")}
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

// ── ErrorBoundary class component ────────────────────────────────────────

export class ErrorBoundary extends React.Component<ErrorBoundaryProps, ErrorBoundaryState> {
  // eslint-disable-next-line @typescript-eslint/no-useless-constructor
  constructor(props: ErrorBoundaryProps) {
    super(props);
    this.state = { hasError: false, error: null, autoRetried: false };
  }

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { hasError: true, error, autoRetried: false };
  }

  componentDidCatch(error: Error, info: React.ErrorInfo) {
    console.error("[ErrorBoundary] Caught render error:", error, info);

    // 首次崩溃时自动尝试恢复（重新挂载子组件），避免无限重试循环
    if (!this.state.autoRetried) {
      this.setState({ autoRetried: true });
      // 延迟一小段时间让错误渲染完成，再自动恢复
      setTimeout(() => this.handleRetry(), 1500);
    }
  }

  handleRetry = () => {
    this.setState({ hasError: false, error: null, autoRetried: false });
  };

  handleRefresh = () => {
    window.location.reload();
  };

  render() {
    if (this.state.hasError) {
      return (
        <ErrorDisplay
          error={this.state.error}
          autoRetried={this.state.autoRetried}
          onRetry={this.handleRetry}
          onRefresh={this.handleRefresh}
        />
      );
    }

    return this.props.children;
  }
}

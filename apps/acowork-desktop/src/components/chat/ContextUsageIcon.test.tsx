import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ContextUsageIcon } from "./ContextUsageIcon";

const mocks = vi.hoisted(() => ({
  chatState: {
    agentStates: {
      "agent-1": {
        sessionStates: {
          "session-1": {
            contextUsage: {
              context_window: 262_100,
              input_tokens: 140_000,
              output_tokens: 4_900,
              total_tokens: 200_000,
              usable_context: 262_100,
              usage_percent: 55.3,
            },
            isCompacting: false,
            provider: "openai" as const,
            sessionStatus: { status: "idle" } as const,
          },
        },
      },
    },
    sendCompressAction: vi.fn(),
  },
  debugState: {
    debugAgentId: "agent-1",
    sessionStates: {
      "session-1": {
        snapshots: [
          {
            iteration: 7,
            built_at: "2026-01-01T00:00:00.000Z",
            sections: [
              { key: "system_prompt", size_bytes: 11_010 },
              { key: "tool_definitions", size_bytes: 29_910 },
              { key: "messages", size_bytes: 391_020 },
              { key: "skill_instructions", size_bytes: 3_150 },
            ],
            total_token_estimate: 145_030,
            phase: "LlmCall" as const,
            request_params: { model: "test-model" },
          },
        ],
      },
    },
  },
  t: (key: string): string => {
    const translations: Record<string, string> = {
      "contextUsage.title": "上下文用量",
      "contextUsage.close": "关闭",
      "contextUsage.used": "已使用",
      "contextUsage.usageBarLabel": "分类用量占比",
      "contextUsage.categories.systemPrompt": "系统提示词",
      "contextUsage.categories.tools": "工具定义",
      "contextUsage.categories.messages": "对话消息",
      "contextUsage.categories.connectors": "连接器",
      "contextUsage.categories.skills": "技能",
      "contextUsage.compressing": "压缩中...",
      "contextUsage.compressSummary": "压缩整个上下文",
      "contextUsage.cacheHitLabel": "缓存命中率",
    };
    return translations[key] ?? key;
  },
}));

vi.mock("../../stores/chatStore", () => ({
  useChatStore: (selector: (state: unknown) => unknown) => selector(mocks.chatState),
}));

vi.mock("../../stores/debugStore", () => ({
  useDebugStore: (selector: (state: unknown) => unknown) => selector(mocks.debugState),
}));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: mocks.t }),
}));

describe("ContextUsageIcon", () => {
  beforeEach(() => {
    mocks.chatState.sendCompressAction.mockReset();
  });

  it("renders categorized usage from the latest debug snapshot and can be closed", () => {
    const { container } = render(
      <ContextUsageIcon agentId="agent-1" sessionId="session-1" />,
    );

    fireEvent.mouseEnter(container.firstElementChild!);

    expect(screen.getByRole("dialog", { name: "上下文用量" })).toBeTruthy();
    expect(screen.getByText("55.3%")).toBeTruthy();
    expect(screen.getByText("系统提示词")).toBeTruthy();
    expect(screen.getByText("工具定义")).toBeTruthy();
    expect(screen.getByText("对话消息")).toBeTruthy();
    expect(screen.getByText("连接器")).toBeTruthy();
    expect(screen.getByText("技能")).toBeTruthy();
    expect(screen.getByText("1.4%")).toBeTruthy();
    expect(screen.getByText("3.8%")).toBeTruthy();
    expect(screen.getByText("49.7%")).toBeTruthy();
    expect(screen.getByText("0.4%")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "关闭" }));
    expect(screen.queryByRole("dialog", { name: "上下文用量" })).toBeNull();
  });

  it("uses the shared dropdown surface tokens so light/dark mode both work", () => {
    const { container } = render(
      <ContextUsageIcon agentId="agent-1" sessionId="session-1" />,
    );
    fireEvent.mouseEnter(container.firstElementChild!);

    const dialog = screen.getByRole("dialog", { name: "上下文用量" });
    // Theme-aware palette, NOT a hardcoded black background, and the
    // same width / radius / border as sibling toolbar dropdowns.
    expect(dialog.className).toContain("bg-modal-surface");
    expect(dialog.className).toContain("rounded-md");
    expect(dialog.className).toContain("border-zinc-200");
    expect(dialog.className).toContain("dark:border-zinc-700");
    expect(dialog.className).not.toContain("bg-[#202126]");
    expect(dialog.className).not.toContain("rounded-[22px]");
  });

  it("shows the cache-hit row when provider cache accounting is present", () => {
    // The input-box usage popup shows the session-lifetime (cumulative)
    // window — the same window the right-hand agent-status panel and
    // the bottom status bar use. We feed cumulative totals and assert
    // the ratio is computed from those, not from the per-turn fields.
    mocks.chatState.agentStates["agent-1"].sessionStates["session-1"].contextUsage = {
      ...mocks.chatState.agentStates["agent-1"].sessionStates["session-1"].contextUsage,
      cache_read_tokens: 5_000, // per-turn — must be ignored
      input_tokens: 12_345, // per-turn — must be ignored
      total_cache_read_tokens: 90_000,
      total_input_tokens: 144_900,
    };

    const { container } = render(
      <ContextUsageIcon agentId="agent-1" sessionId="session-1" />,
    );
    fireEvent.mouseEnter(container.firstElementChild!);

    // OpenAI protocol, cumulative: 90K / 144.9K ≈ 0.6211 → "62%".
    expect(screen.getByText("62%")).toBeTruthy();
    expect(screen.getByText("缓存命中率")).toBeTruthy();
    expect(screen.getByText("90.0K")).toBeTruthy();
    expect(screen.getByText("144.9K")).toBeTruthy();
  });

  it("dispatches a Summary(1) compress action when the button is clicked", () => {
    const { container } = render(
      <ContextUsageIcon agentId="agent-1" sessionId="session-1" />,
    );
    fireEvent.mouseEnter(container.firstElementChild!);

    fireEvent.click(screen.getByRole("button", { name: "压缩整个上下文" }));
    expect(mocks.chatState.sendCompressAction).toHaveBeenCalledWith(
      "agent-1",
      "session-1",
      1, // CompressType::SUMMARY
    );
  });

  it("disables the compress button while the session is busy", () => {
    mocks.chatState.agentStates["agent-1"].sessionStates["session-1"].sessionStatus =
      { status: "running" } as const;

    const { container } = render(
      <ContextUsageIcon agentId="agent-1" sessionId="session-1" />,
    );
    fireEvent.mouseEnter(container.firstElementChild!);

    const btn = screen.getByRole("button", { name: "压缩整个上下文" });
    expect((btn as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(btn);
    expect(mocks.chatState.sendCompressAction).not.toHaveBeenCalled();

    // Restore so other tests aren't affected.
    mocks.chatState.agentStates["agent-1"].sessionStates["session-1"].sessionStatus =
      { status: "idle" } as const;
  });
});

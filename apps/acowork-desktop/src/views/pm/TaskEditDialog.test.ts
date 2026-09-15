/**
 * Regression tests for `buildAgentOptions` (TaskEditDialog assignee dropdown).
 *
 * ADR-073 background:
 *   Before ADR-073 was fully applied to acowork-pm, the assignee dropdown
 *   used `meta.agent_id` (the package id, e.g. "com.acowork.ponytail") as
 *   its `value`. Meanwhile the agentStore keys entries by `meta.instance_id`
 *   (a UUID v4). The two were out of sync, so the dropdown round-trip was
 *   broken: the displayed selection (UUID) never matched the saved task's
 *   `assignee` (package id) when the user changed selection.
 *
 *   Fix: dropdown `value` is now `meta.instance_id`, matching
 *   `task.assignee` storage. This file pins the mapping.
 */

import { describe, it, expect } from "vitest";
import { buildAgentOptions } from "./TaskEditDialog";

describe("buildAgentOptions (ADR-073 assignee dropdown)", () => {
  it("uses instance_id as value (NOT package agent_id)", () => {
    const agents = [
      {
        meta: {
          instance_id: "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d",
          agent_id: "com.acowork.ponytail",
          display_name: "Ponytail",
          name: "Senior Engineer",
        },
      },
    ];
    const opts = buildAgentOptions(agents);
    expect(opts).toHaveLength(1);
    expect(opts[0].value).toBe("3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d");
    // Regression: must NOT leak the package id as the option value.
    expect(opts[0].value).not.toBe("com.acowork.ponytail");
    expect(opts[0].value).not.toContain("com.acowork");
  });

  it("prefers display_name over name over agent_id for label", () => {
    const agents = [
      { meta: { instance_id: "u1", agent_id: "p1", display_name: "Ponytail Display", name: "Ponytail Name" } },
      { meta: { instance_id: "u2", agent_id: "p2", display_name: undefined, name: "Architect Name" } },
      { meta: { instance_id: "u3", agent_id: "p3", display_name: undefined, name: undefined } },
    ];
    const opts = buildAgentOptions(agents);
    expect(opts[0].label).toBe("Ponytail Display");
    expect(opts[1].label).toBe("Architect Name");
    expect(opts[2].label).toBe("p3"); // 最后 fallback to agent_id
  });

  it("keeps multiple instances of the same package as distinct options", () => {
    // ADR-073 invariant 1: same package can have multiple instances, each
    // with its own UUID. The dropdown must show them as separate options,
    // not collapse them (which the old code did when keyed by agent_id).
    const sharedPackage = "com.acowork.architect";
    const agents = [
      { meta: { instance_id: "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d", agent_id: sharedPackage, display_name: "Architect (workspace-a)" } },
      { meta: { instance_id: "a91b07e4-c2d3-4f8b-a91b-07e4c2d34f8b", agent_id: sharedPackage, display_name: "Architect (workspace-b)" } },
    ];
    const opts = buildAgentOptions(agents);
    expect(opts).toHaveLength(2);
    expect(opts[0].value).not.toBe(opts[1].value);
    expect(opts[0].label).toBe("Architect (workspace-a)");
    expect(opts[1].label).toBe("Architect (workspace-b)");
    // Both values are valid UUIDs (no package id leakage)
    for (const opt of opts) {
      expect(opt.value).not.toBe(sharedPackage);
    }
  });

  it("returns empty array for empty agent list", () => {
    expect(buildAgentOptions([])).toEqual([]);
  });

  it("downstream consumer can round-trip selection to task.assignee", () => {
    // Simulate: user selects the first option, code reads `opts[0].value`
    // and sends it as task.assignee. The persisted assignee must be the
    // instance_id (UUID), not the display label.
    const agents = [
      { meta: { instance_id: "5d2e1100-7e4b-4d2a-b6f1-1a91b07e4c2d", agent_id: "com.acowork.x", display_name: "X Bot" } },
    ];
    const opts = buildAgentOptions(agents);
    const selectedValue = opts[0].value;
    // The saved assignee is the UUID, never the display name.
    expect(selectedValue).toMatch(/^[0-9a-f-]{36}$/);
    expect(selectedValue).not.toBe("X Bot");
  });

  it("filters to project members only when memberIds is provided (linked assignment)", () => {
    // 联动指派：assignee 必须是项目成员。非成员 Agent 不得出现在选项中。
    const agents = [
      { meta: { instance_id: "11111111-1111-1111-1111-111111111111", agent_id: "com.acowork.member", display_name: "Member Agent" } },
      { meta: { instance_id: "22222222-2222-2222-2222-222222222222", agent_id: "com.acowork.nonmember", display_name: "Not A Member" } },
    ];
    const memberIds = new Set(["11111111-1111-1111-1111-111111111111"]);
    const opts = buildAgentOptions(agents, memberIds);
    expect(opts).toHaveLength(1);
    expect(opts[0].value).toBe("11111111-1111-1111-1111-111111111111");
    expect(opts[0].label).toBe("Member Agent");
    // 非成员不得泄漏进下拉
    expect(opts[0].value).not.toBe("22222222-2222-2222-2222-222222222222");
  });

  it("returns all agents when onlyIds is null/undefined (backward compatible)", () => {
    const agents = [
      { meta: { instance_id: "u1", agent_id: "p1", display_name: "A" } },
      { meta: { instance_id: "u2", agent_id: "p2", display_name: "B" } },
    ];
    expect(buildAgentOptions(agents)).toHaveLength(2);
    expect(buildAgentOptions(agents, null)).toHaveLength(2);
  });
});
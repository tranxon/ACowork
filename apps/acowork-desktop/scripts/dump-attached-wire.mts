#!/usr/bin/env node
// scripts/dump-attached-wire.mts
//
// Lock the desktop → runtime MQTT wire contract for `attached_items`.
//
// Run with:
//   tsx apps/acowork-desktop/scripts/dump-attached-wire.mts
//
// Output:
//   ../../core/acowork-core/tests/fixtures/desktop_attached_items.json
//
// `core/acowork-core/tests/attached_items_wire.rs` reads that fixture
// back and asserts each entry deserializes into
// `acowork_core::protocol::AttachedItem`. Re-run this script whenever
// the wire shape changes; the Rust test will fail loudly until both
// sides match.
//
// WHY: the runtime parses each `attached_items[]` entry via
// `serde_json::from_value::<AttachedItem>(...).ok()` at
// `gateway_loop.rs:813-820` — failures are silently filtered out, so
// a shape mismatch surfaced as "every attachment vanishes after send"
// rather than a diagnostic. This script + the Rust test make the
// mismatch impossible to ship undetected.

import { writeFileSync, mkdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve, join } from "node:path";

import { toWireAttachedItems, type AttachedItem } from "../src/lib/types.ts";

// ── Hand-picked fixtures covering all 5 variants + every Option / numeric /
//    string corner case. Each entry uses values that look like real chat
//    payloads (hex document ids, real file paths, line numbers, byte counts)
//    so the fixture is grepable for both humans and the Rust parser. ─────────

const FIXTURES: AttachedItem[] = [
  // 1. file_upload — with clientId (optimistic insertion path)
  {
    type: "file_upload",
    documentId: "0123456789ab-3",
    filename: "Q3-report.pdf",
    format: "pdf",
    sizeBytes: 482301,
    clientId: "msg-a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  },
  // 2. image_upload — with width / height + clientId
  {
    type: "image_upload",
    documentId: "fedcba987654-7",
    filename: "screen.png",
    format: "png",
    sizeBytes: 987654,
    width: 1920,
    height: 1080,
    clientId: "msg-b2c3d4e5-f6a7-8901-bcde-f12345678901",
  },
  // 3. image_upload — CLI-style, width/height omitted, NO clientId
  //    (backward-compatibility: old clients don't send clientId)
  {
    type: "image_upload",
    documentId: "112233445566-a",
    filename: "diagram.jpg",
    format: "jpg",
    sizeBytes: 12345,
  },
  // 4. attached_file — workspace reference with clientId
  {
    type: "attached_file",
    absPath: "/Users/alice/projects/agentcow/core/acowork-runtime/src/lib.rs",
    name: "lib.rs",
    clientId: "msg-c3d4e5f6-a7b8-9012-cdef-123456789012",
  },
  // 5. attached_selection — line-range variant with clientId
  {
    type: "attached_selection",
    absPath: "/Users/alice/projects/agentcow/core/acowork-runtime/src/agent/loop_.rs",
    name: "loop_.rs",
    startLine: 521,
    endLine: 540,
    clientId: "msg-d4e5f6a7-b8c9-0123-defa-234567890123",
  },
  // 6. attached_selection — single-line collapse (start == end), NO clientId
  {
    type: "attached_selection",
    absPath: "/Users/alice/projects/agentcow/core/acowork-runtime/src/main.rs",
    name: "main.rs",
    startLine: 42,
    endLine: 42,
  },
  // 7. attached_folder — workspace dir reference, NO clientId
  {
    type: "attached_folder",
    absPath: "/Users/alice/projects/agentcow/core/acowork-runtime/src/agent/session",
    name: "session",
  },
] as const;

// Serialize exactly as the chatStore does at the MQTT boundary. The Rust
// parser sees this byte-for-byte; we keep the array formatting minimal so
// key differences are easy to grep (no whitespace skew between variants).
const wire = toWireAttachedItems(FIXTURES);
const out = JSON.stringify(wire, null, 2) + "\n";

const here = dirname(fileURLToPath(import.meta.url));
const outPath = resolve(
  here,
  "..",
  "..",
  "..",
  "core",
  "acowork-core",
  "tests",
  "fixtures",
  "desktop_attached_items.json",
);
mkdirSync(dirname(outPath), { recursive: true });
writeFileSync(outPath, out, "utf8");

// ── Wire-contract sanity checks. These fail loudly and immediately if a
//    future field rename or rename_all change breaks the shape — useful
//    even though the Rust test is the authoritative cross-language check.
//
//    Required keys MUST be present. Optional keys (e.g. `width` /
//    `height` on `image_upload`) are tolerated; the Rust side accepts
//    them because the structs don't set `deny_unknown_fields`. The bug
//    we're guarding against is **snake_case residue** on required keys,
//    which causes the runtime's `from_value::<AttachedItem>().ok()`
//    filter to silently drop the entry. ──

const requiredWireShapes: Record<string, readonly string[]> = {
  file_upload: ["type", "documentId", "filename", "format", "sizeBytes"],
  image_upload: ["type", "documentId", "filename", "format", "sizeBytes"],
  attached_file: ["type", "absPath", "name"],
  attached_selection: ["type", "absPath", "name", "startLine", "endLine"],
  attached_folder: ["type", "absPath", "name"],
};

let failed = false;
wire.forEach((entry, i) => {
  const e = entry as Record<string, unknown>;
  const type = e.type as string;
  const required = requiredWireShapes[type];
  if (!required) {
    console.error(`#${i}: unknown type ${type}`);
    failed = true;
    return;
  }
  const present = Object.keys(e);
  const missing = required.filter((k) => !present.includes(k));
  if (missing.length > 0) {
    console.error(
      `#${i} (${type}): missing required keys [${missing.join(",")}]`,
    );
    failed = true;
  }
  // Reject any snake_case residue on ANY key (required or optional).
  // The bug we're guarding against is the silent-drop on
  // `from_value::<AttachedItem>` — only required keys cause a drop,
  // but a snake_case optional key would also be ignored by serde's
  // camelCase deserializer and is a strong signal the shape drifted.
  for (const k of present) {
    if (/^[a-z]+_[a-z]/.test(k)) {
      console.error(`#${i} (${type}): snake_case field name "${k}" — runtime would silently drop this item`);
      failed = true;
    }
  }
});

if (failed) {
  process.exit(1);
}

console.log(`✔ wrote ${wire.length} entries → ${outPath}`);

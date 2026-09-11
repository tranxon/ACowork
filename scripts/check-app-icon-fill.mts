#!/usr/bin/env -S node --experimental-strip-types
/**
 * Self-check: every regenerated Tauri icon's yellow rounded square must
 * extend to within 1 px of the canvas edge on each side. If the icon's
 * background ever shrinks back inside the canvas (i.e. someone re-runs
 * `tauri icon` from a source SVG that still has brand-mark margins), this
 * fails and prints the offending file.
 *
 * Run: node scripts/check-app-icon-fill.mts
 *      (from repo root, no deps — Node 22+ with --experimental-strip-types)
 */
import { readdirSync, readFileSync } from "node:fs";
import { join, extname } from "node:path";

const ICONS_DIR = join(
  process.cwd(),
  "apps/acowork-desktop/src-tauri/icons",
);

type Stats = {
  file: string;
  size: [number, number];
  margins: [number, number, number, number]; // T,B,L,R
};

// Inline minimal RGBA PNG reader using the built-in zlib + manual chunk parsing,
// so this script has zero npm dependencies. Only supports the 8-bit RGBA format
// that `tauri icon` emits.
import { inflateSync } from "node:zlib";

function readPngRGBA(path: string): { width: number; height: number; pixels: Uint8Array } {
  const raw = readFileSync(path);
  // PNG signature: 8 bytes
  if (raw[0] !== 0x89 || raw[1] !== 0x50 || raw[2] !== 0x4e || raw[3] !== 0x47) {
    throw new Error(`not a PNG: ${path}`);
  }
  let p = 8;
  let width = 0, height = 0, bitDepth = 0, colorType = 0;
  let idat = Buffer.alloc(0);
  while (p < raw.length) {
    const len = raw.readUInt32BE(p); p += 4;
    const type = raw.toString("ascii", p, p + 4); p += 4;
    const data = raw.subarray(p, p + len); p += len;
    p += 4; // CRC
    if (type === "IHDR") {
      width = data.readUInt32BE(0);
      height = data.readUInt32BE(4);
      bitDepth = data[8];
      colorType = data[9];
    } else if (type === "IDAT") {
      idat = Buffer.concat([idat, data]);
    } else if (type === "IEND") {
      break;
    }
  }
  if (bitDepth !== 8 || colorType !== 6) {
    throw new Error(`unsupported PNG format (depth=${bitDepth}, color=${colorType}): ${path}`);
  }
  const decompressed = inflateSync(idat);
  const stride = width * 4;
  const out = new Uint8Array(width * height * 4);
  let prevRow = new Uint8Array(stride);
  let rp = 0;
  for (let y = 0; y < height; y++) {
    const filter = decompressed[rp++];
    const row = new Uint8Array(stride);
    const raw = decompressed.subarray(rp, rp + stride);
    rp += stride;
    for (let x = 0; x < stride; x++) {
      const cur = raw[x];
      const left = x >= 4 ? row[x - 4] : 0;
      const up = prevRow[x];
      const upLeft = x >= 4 ? prevRow[x - 4] : 0;
      let v: number;
      switch (filter) {
        case 0: v = cur; break;
        case 1: v = (cur + left) & 0xff; break;
        case 2: v = (cur + up) & 0xff; break;
        case 3: v = (cur + ((left + up) >> 1)) & 0xff; break;
        case 4: {
          const pa = Math.abs(up - upLeft);
          const pb = Math.abs(left - upLeft);
          const pc = Math.abs(left + up - 2 * upLeft);
          const pred = pa <= pb && pa <= pc ? left : pb <= pc ? up : upLeft;
          v = (cur + pred) & 0xff;
          break;
        }
        default: throw new Error(`bad filter ${filter} at y=${y} in ${path}`);
      }
      row[x] = v;
    }
    out.set(row, y * stride);
    prevRow = row;
  }
  return { width, height, pixels: out };
}

function yellowMargins(path: string): Stats {
  const { width, height, pixels } = readPngRGBA(path);
  let minX = width, minY = height, maxX = -1, maxY = -1;
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      const i = (y * width + x) * 4;
      const r = pixels[i], g = pixels[i + 1], b = pixels[i + 2], a = pixels[i + 3];
      // brand yellow band from the gradient (#F7C948 → #F0A92E)
      if (a > 0 && r > 200 && g > 150 && b < 100) {
        if (x < minX) minX = x;
        if (y < minY) minY = y;
        if (x > maxX) maxX = x;
        if (y > maxY) maxY = y;
      }
    }
  }
  if (maxX < 0) throw new Error(`no yellow pixels found in ${path}`);
  return {
    file: path,
    size: [width, height],
    margins: [minY, height - 1 - maxY, minX, width - 1 - maxX],
  };
}

function walkPngs(dir: string): string[] {
  const out: string[] = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, entry.name);
    if (entry.isDirectory()) out.push(...walkPngs(p));
    else if (extname(entry.name) === ".png") out.push(p);
  }
  return out;
}

// Android legacy ic_launcher.png + ic_launcher_round.png keep tauri CLI's
// intentional ~8% Android launcher padding. Adaptive-icon foreground,
// desktop, Windows Store and iOS sets must all be edge-to-edge.
const ANDROID_PADDING_FILES = new Set([
  "ic_launcher.png",
  "ic_launcher_round.png",
]);

const files = walkPngs(ICONS_DIR).sort();
const failures: Array<{ file: string; margins: [number, number, number, number]; reason: string }> = [];

for (const file of files) {
  const base = file.split(/[\\/]/).pop()!;
  if (ANDROID_PADDING_FILES.has(base)) continue;
  const stats = yellowMargins(file);
  const [t, b, l, r] = stats.margins;
  const max = Math.max(t, b, l, r);
  if (max > 1) {
    failures.push({ file, margins: stats.margins, reason: `yellow rect leaves ${max}px gap to canvas edge` });
  }
}

if (failures.length > 0) {
  console.error("FAIL: icons still have transparent margin around the yellow rect:");
  for (const f of failures) {
    console.error(`  ${f.file}\n    margins T/B/L/R = ${f.margins.join("/")} — ${f.reason}`);
  }
  process.exit(1);
}

console.log(`OK: ${files.length} icon files checked; yellow rect fills canvas edge-to-edge (≤1px rounding tolerance).`);
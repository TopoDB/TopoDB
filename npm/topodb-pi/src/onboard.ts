// src/onboard.ts — AGENTS.md pointer injection: pure fence logic + a
// best-effort fetch-and-write, mirroring plugins/claude-code/hooks/onboard.js
// and crates/topodb-onboarding/src/fence.rs. Keep all three in lockstep.
//
// pi's AGENTS.md plays the role CLAUDE.md plays for the Claude Code plugin —
// same fence markers, same outcome semantics, different target filename.

import { readFileSync, writeFileSync, renameSync, unlinkSync, existsSync } from "node:fs";
import path from "node:path";
import type { TopodbServer } from "./server-handle.ts";

const START_PREFIX = "<!-- topodb:pointer:start version=";
const END_MARKER = "<!-- topodb:pointer:end -->";

export type FenceOutcome = "injected" | "replaced" | "unchanged" | "skipped";

/**
 * Insert, replace, or skip the topodb pointer fence in `existing` text.
 * Returns { text, outcome } where outcome is one of:
 *   "injected"  — no fence present, block appended.
 *   "replaced"  — fence present with an older (or malformed/unparsable,
 *                 which counts as 0) version; block replaces it in place.
 *   "unchanged" — fence present with version >= the new version.
 *   "skipped"   — exactly one marker present, or markers in reversed order
 *                 (end before start) — treat as corrupted, don't touch it.
 */
export function upsertFence(
  existing: string,
  block: string,
  version: number,
): { text: string; outcome: FenceOutcome } {
  const s = existing.indexOf(START_PREFIX);
  const e = existing.indexOf(END_MARKER);

  if (s === -1 && e === -1) {
    const sep = existing === "" || existing.endsWith("\n\n") ? "" : existing.endsWith("\n") ? "\n" : "\n\n";
    return { text: `${existing}${sep}${block}`, outcome: "injected" };
  }

  if (s !== -1 && e !== -1 && e > s) {
    const after = existing.slice(s + START_PREFIX.length);
    const m = after.match(/^[0-9]+/);
    let existingVersion = m ? parseInt(m[0], 10) : 0;
    if (!Number.isInteger(existingVersion) || existingVersion > 4294967295) existingVersion = 0;
    if (existingVersion >= version) {
      return { text: existing, outcome: "unchanged" };
    }
    const endFull = e + END_MARKER.length;
    // Include a trailing newline if the old block carried one.
    const tail = existing[endFull] === "\n" ? existing.slice(endFull + 1) : existing.slice(endFull);
    const head = existing.slice(0, s);
    // Caller-supplied block is always terminated by exactly one '\n', so
    // trim then re-add exactly one — mirrors the Rust/JS siblings.
    const body = block.replace(/\n+$/, "");
    return { text: `${head}${body}\n${tail}`, outcome: "replaced" };
  }

  // Exactly one marker present, or reversed order — corrupted; leave as-is.
  return { text: existing, outcome: "skipped" };
}

// Atomic tmp+rename write, mirroring onboard.js's atomicWrite (same bounded
// linear backoff for the Windows EPERM/EACCES rename race).
function atomicWrite(file: string, contents: string): void {
  const tmp = `${file}.${process.pid}.tmp`;
  writeFileSync(tmp, contents);
  const sleeper = new Int32Array(new SharedArrayBuffer(4));
  for (let attempt = 0; ; attempt++) {
    try {
      renameSync(tmp, file);
      return;
    } catch (err) {
      const code = (err as NodeJS.ErrnoException).code;
      if ((code !== "EPERM" && code !== "EACCES") || attempt >= 20) {
        try {
          unlinkSync(tmp);
        } catch {
          /* best effort cleanup */
        }
        throw err;
      }
      Atomics.wait(sleeper, 0, 0, 5 + attempt * 5);
    }
  }
}

/**
 * Best-effort: fetch the onboarding pointer over the given `server` and
 * upsert it into <cwd>/AGENTS.md. Swallows every error (call failure,
 * unexpected result shape, unreadable/unwritable file) — never throws, so a
 * caller on session_start's hot path can call this unconditionally.
 */
export async function injectPointer(cwd: string, server: TopodbServer): Promise<void> {
  try {
    const res = (await server.call("onboarding_pointer", {})) as { pointer?: unknown; version?: unknown };
    const block = res?.pointer;
    const version = res?.version;
    if (typeof block !== "string" || typeof version !== "number") return;

    const file = path.join(cwd, "AGENTS.md");
    const existing = existsSync(file) ? readFileSync(file, "utf8") : "";
    const { text, outcome } = upsertFence(existing, block, version);
    if (outcome === "injected" || outcome === "replaced") {
      atomicWrite(file, text);
    }
  } catch {
    // best-effort — never let onboarding injection break session start.
  }
}

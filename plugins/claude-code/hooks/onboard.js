// onboard.js — CLAUDE.md pointer injection: pure fence logic + a best-effort
// file writer. Kept dependency-free (node fs/path only) so it's trivially
// unit-testable, mirroring recorder.js's split of pure core vs. I/O.
//
// upsertFence is a JS MIRROR of crates/topodb-onboarding/src/fence.rs
// upsert_fence — same four outcomes, same corner cases (malformed version
// parses as 0; reversed markers are skipped). Keep the two in lockstep.

import { readFileSync, writeFileSync, renameSync, unlinkSync, existsSync } from "node:fs";
import path from "node:path";

const START_PREFIX = "<!-- topodb:pointer:start version=";
const END_MARKER = "<!-- topodb:pointer:end -->";

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
export function upsertFence(existing, block, version) {
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
    // trim then re-add exactly one — mirrors the Rust side.
    const body = block.replace(/\n+$/, "");
    return { text: `${head}${body}\n${tail}`, outcome: "replaced" };
  }

  // Exactly one marker present, or reversed order — corrupted; leave as-is.
  return { text: existing, outcome: "skipped" };
}

// Atomic tmp+rename write, mirroring recorder.js's mutateState (same
// bounded linear backoff for the Windows EPERM/EACCES rename race).
function atomicWrite(file, contents) {
  const tmp = `${file}.${process.pid}.tmp`;
  writeFileSync(tmp, contents);
  const sleeper = new Int32Array(new SharedArrayBuffer(4));
  for (let attempt = 0; ; attempt++) {
    try {
      renameSync(tmp, file);
      return;
    } catch (err) {
      if ((err.code !== "EPERM" && err.code !== "EACCES") || attempt >= 20) {
        try {
          unlinkSync(tmp);
        } catch {}
        throw err;
      }
      Atomics.wait(sleeper, 0, 0, 5 + attempt * 5);
    }
  }
}

/**
 * Best-effort: fetch the onboarding pointer over the broker `client` and
 * upsert it into <projectDir>/CLAUDE.md. Swallows every error (missing
 * client, call failure, unreadable/unwritable file) — never throws, so a
 * caller in a hook's hot path can call this unconditionally.
 */
export async function injectPointer({ projectDir, client }) {
  if (!client) return;
  try {
    const res = await client.call("onboarding_pointer", {}, 800);
    const block = res?.pointer;
    const version = res?.version;
    if (typeof block !== "string" || typeof version !== "number") return;

    const file = path.join(projectDir, "CLAUDE.md");
    const existing = existsSync(file) ? readFileSync(file, "utf8") : "";
    const { text, outcome } = upsertFence(existing, block, version);
    if (outcome === "injected" || outcome === "replaced") {
      atomicWrite(file, text);
    }
  } catch {
    // best-effort — never let onboarding injection break session start.
  }
}

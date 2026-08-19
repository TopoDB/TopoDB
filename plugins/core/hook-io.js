// hook-io.js — the boring, shared half of every hook script: stdin, JSON,
// kill switches, the debug dump, and the "exit 0 no matter what" runner.
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";

export function readStdin() {
  return new Promise((r) => {
    let buf = "";
    process.stdin.on("data", (d) => (buf += d));
    process.stdin.on("end", () => r(buf));
  });
}
export function offSwitch(v) {
  const s = String(v ?? "").toLowerCase();
  return s === "0" || s === "off";
}
export function recordingDisabled(env) { return offSwitch(env.TOPODB_RECORDING); }
export function parseJson(raw) {
  try { const v = JSON.parse(raw); return v && typeof v === "object" && !Array.isArray(v) ? v : null; } catch { return null; }
}
/** Debug escape (TOPODB_HOOK_DEBUG=1): dump the raw stdin payload so a real
 *  session can pin the true hook payload shape. Best-effort — never throws. */
export function debugDump({ dataDir, env, eventName, raw }) {
  try {
    if (!env?.TOPODB_HOOK_DEBUG || !dataDir) return;
    const dir = path.join(dataDir, "episodes");
    mkdirSync(dir, { recursive: true });
    const safe = String(eventName ?? "unknown").replace(/[^A-Za-z0-9_-]/g, "_");
    writeFileSync(path.join(dir, `debug-${safe}.json`), raw ?? "");
  } catch { /* best-effort only */ }
}
/** Run a hook main(): self-deadline, swallow everything, exit 0 always. */
export function runHook(main, { deadlineMs = 2500 } = {}) {
  const guard = setTimeout(() => process.exit(0), deadlineMs);
  Promise.resolve()
    .then(main)
    .catch(() => {})
    .finally(() => { clearTimeout(guard); process.exit(0); });
}

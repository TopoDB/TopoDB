// Teardown helper for test dirs that (may) contain live broker/server
// processes. Windows cannot unlink a running executable (EPERM) and keeps
// unlinked-but-open files in delete-pending (parent rmdir → ENOTEMPTY), so
// a plain rmSync races the broker's idle exit. Retry with real sleeps; if
// the dir STILL cannot be removed, throw an error naming every surviving
// entry — the evidence needed to see WHAT was held, not just that
// something was.
import { rmSync, readdirSync } from "node:fs";
import path from "node:path";

function survivors(dir, prefix = "") {
  let out = [];
  let names;
  try {
    names = readdirSync(dir, { withFileTypes: true });
  } catch (err) {
    return [`${prefix}<unlistable: ${err.code}>`];
  }
  for (const d of names) {
    const rel = prefix ? `${prefix}/${d.name}` : d.name;
    if (d.isDirectory()) out = out.concat(survivors(path.join(dir, d.name), rel));
    else out.push(rel);
  }
  return out.length ? out : [`${prefix}/<empty-but-undeletable>`];
}

function graceError(dir, lastErr, attempts, delayMs) {
  const held = survivors(dir).slice(0, 20).join(", ");
  return new Error(
    `${lastErr?.message ?? "rm failed"}; still present after ${attempts}x${delayMs}ms: [${held}]`,
  );
}

export async function rmWithGrace(dir, { attempts = 30, delayMs = 1000 } = {}) {
  let lastErr = null;
  for (let i = 0; i < attempts; i++) {
    try {
      rmSync(dir, { recursive: true, force: true });
      return;
    } catch (err) {
      lastErr = err;
      await new Promise((r) => setTimeout(r, delayMs));
    }
  }
  throw graceError(dir, lastErr, attempts, delayMs);
}

// Synchronous sibling for teardowns that can't await (sync `finally` blocks —
// e.g. broker.test.js's many cleanup sites). Same budget and diagnostics; the
// inter-attempt wait is a real sleep via Atomics.wait (blocks this thread
// without busy-spinning the CPU), not a busy loop. Node >= 22 (the plugin's
// engines floor) always has SharedArrayBuffer/Atomics.
export function rmWithGraceSync(dir, { attempts = 30, delayMs = 1000 } = {}) {
  const clock = new Int32Array(new SharedArrayBuffer(4));
  let lastErr = null;
  for (let i = 0; i < attempts; i++) {
    try {
      rmSync(dir, { recursive: true, force: true });
      return;
    } catch (err) {
      lastErr = err;
      Atomics.wait(clock, 0, 0, delayMs); // sleep delayMs (value never changes → always times out)
    }
  }
  throw graceError(dir, lastErr, attempts, delayMs);
}

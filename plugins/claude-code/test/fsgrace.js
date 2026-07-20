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
  const held = survivors(dir).slice(0, 20).join(", ");
  throw new Error(`${lastErr?.message ?? "rm failed"}; still present after ${attempts}x${delayMs}ms: [${held}]`);
}

#!/usr/bin/env node
// SessionStart: inject the project's recent memories as context.
// HARD RULES: exit 0 no matter what; nothing on stdout except the payload;
// self-deadline (this hook BLOCKS session start); main sessions only.
import { pathToFileURL } from "node:url";
import { connectForProject } from "../core/broker-client.js";
import { recallForSessionStart } from "../core/hooks/recall.js";
import { injectPointer } from "./onboard.js";
import { sessionScopes } from "../core/server-args.js";
import { tryMarker } from "../core/warehouse-spool.js";
import { readStdin, parseJson } from "../core/hook-io.js";

export { renderHealth, renderInjection } from "../core/hooks/recall.js";

const DEADLINE_MS = 2500;

async function main() {
  const raw = await readStdin();
  const payload = parseJson(raw);
  if (!payload) return;
  if (payload.agent_id || payload.agent_type) return; // main sessions only
  if (payload.source !== "startup" && payload.source !== "clear") return;

  const dataDir = process.env.CLAUDE_PLUGIN_DATA;
  const projectDir = process.env.CLAUDE_PROJECT_DIR ?? payload.cwd;
  if (!dataDir || !projectDir) return;

  tryMarker({ dataDir, env: process.env, projectDir, sessionId: payload.session_id, type: "session_start", sessionScopes });

  const client = await connectForProject({ projectDir, dataDir });
  if (!client) return; // no broker yet — first-ever session; next one has it
  try {
    const out = await recallForSessionStart(client);
    if (out) {
      // CHAR_CAP (baked into recallForSessionStart) keeps this comfortably
      // under the ~64KB pipe buffer, so the process.exit(0) in finally()
      // below can never truncate the write mid-flight.
      process.stdout.write(
        JSON.stringify({
          hookSpecificOutput: { hookEventName: "SessionStart", additionalContext: out },
        }),
      );
    }
  } finally {
    // Best-effort CLAUDE.md pointer injection: fully swallows its own
    // errors and never writes to stdout, so it can't affect the memory
    // injection above or the hook's exit/deadline contract. Runs in the
    // finally block (before client.close()) so it fires on every path,
    // including an empty memory store or a recent_memories throw — those
    // are exactly the fresh-install case the onboarding pointer exists for.
    try {
      await injectPointer({ projectDir, client });
    } catch { /* onboarding injection is best-effort; never break the hook */ }
    client.close();
  }
}

// Only run main() when executed as a script — the test imports renderInjection.
// Compared via pathToFileURL (not a hand-built `file://` template) so this
// holds for paths with spaces and on Windows, where a template string
// mismatches the URL's percent-encoding/drive-letter form.
if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const guard = setTimeout(() => process.exit(0), DEADLINE_MS);
  main()
    .catch(() => {})
    .finally(() => {
      clearTimeout(guard);
      process.exit(0);
    });
}

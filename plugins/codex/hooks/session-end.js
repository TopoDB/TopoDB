#!/usr/bin/env node
// SessionEnd: advisory only, never load-bearing — Stop already flushed the
// episode, and SessionEnd also fires after 30 min idle. Codex hard-kills this
// hook at 3 s, so all it does inline is write the session_end spool marker and
// hand any leftover episode state to a DETACHED flusher child (detached +
// unref, stdio ignored — it must never hold this hook's stdio or its exit).
// Background agents and garbage stdin produce nothing; exit 0 no matter what.
import { spawn } from "node:child_process";
import { fileURLToPath, pathToFileURL } from "node:url";
import { readStdin, parseJson, debugDump, runHook } from "../core/hook-io.js";
import { sessionScopes } from "../core/server-args.js";
import { tryMarker } from "../core/warehouse-spool.js";
import { connectForProject } from "../core/broker-client.js";
import { flushEpisode } from "../core/hooks/episode.js";
import { hookContext, HARNESS } from "./_env.js";

const SELF = fileURLToPath(import.meta.url);

async function main() {
  const raw = await readStdin();
  const p = parseJson(raw);
  const { dataDir, projectDir, sessionId } = hookContext(p ?? {}, process.env);
  debugDump({ dataDir, env: process.env, eventName: "SessionEnd", raw });
  if (!p || p.is_background_agent === true) return;
  if (!dataDir || !projectDir || !sessionId) return;
  tryMarker({ dataDir, env: process.env, projectDir, sessionId, type: "session_end", sessionScopes, harness: HARNESS });
  const job = JSON.stringify({ dataDir, projectDir, sessionId, reason: typeof p.reason === "string" ? p.reason : "" });
  spawn(process.execPath, [SELF, "--flush", job], { detached: true, stdio: "ignore" }).unref();
}

// The detached child: a leftover flush is best-effort — no daemon means the
// state stays for a later sweep, exactly as if this child had never run.
async function flushMain(jobJson) {
  const { dataDir, projectDir, sessionId, reason } = JSON.parse(jobJson);
  const timeoutMs = Number(process.env.TOPODB_DAEMON_CONNECT_MS);
  const connect = Number.isFinite(timeoutMs) && timeoutMs > 0
    ? (o) => connectForProject({ ...o, connectTimeoutMs: timeoutMs })
    : connectForProject;
  await flushEpisode({ dataDir, env: process.env, projectDir, sessionId, assistantText: null, reason, connect });
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  if (process.argv[2] === "--flush") flushMain(process.argv[3]).catch(() => {}).finally(() => process.exit(0));
  else runHook(main, { deadlineMs: 2000 });
}

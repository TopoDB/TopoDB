import { sessionScopes } from "../server-args.js";
import { warehouseDisabled, artifactEvent, appendSpool, HARNESS } from "../warehouse-spool.js";
/** Land one raw tool artifact in the warehouse spool. One appendFileSync, no daemon. */
export function captureArtifact({ dataDir, env, projectDir, sessionId, toolName, toolInput, toolResponse, cwd, agent, harness = HARNESS }) {
  if (warehouseDisabled(env ?? {})) return false;
  if (!dataDir || !sessionId || !projectDir) return false;
  const { scope } = sessionScopes({ projectDir });
  const ev = artifactEvent({ toolName, toolInput, toolResponse, sessionId, scope, cwd, agent, harness });
  if (!ev) return false;
  appendSpool(dataDir, sessionId, ev, env ?? {});
  return true;
}

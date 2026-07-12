// The memory model, as the argv topodb-mcp is launched with. Pure and separate
// from launch.js so it is testable without spawning anything.
import path from "node:path";
import { projectScopeId } from "./scope-id.js";

/**
 * Reads span {project, shared}; writes default to the project scope. The bundled
 * skill tells the agent to pass scope:"shared" explicitly when a lesson
 * generalizes beyond this repo.
 *
 * `--allow-unscoped-changes` is deliberately absent, and a test pins that:
 * get_changes is the one unscoped read, and in a database shared across every
 * project it would replay every other project's op log into this session.
 */
export function serverArgs({ projectDir, dataDir }) {
  const scope = projectScopeId(projectDir);
  return [
    "--db",
    path.join(dataDir, "memory.redb"),
    "--scope",
    scope,
    "--read-scopes",
    `${scope},shared`,
  ];
}

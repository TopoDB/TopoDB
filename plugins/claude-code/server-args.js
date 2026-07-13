// The memory model, as the argv topodb-mcp is launched with. Pure and separate
// from launch.js so it is testable without spawning anything.
import path from "node:path";
import { projectScopeId } from "./scope-id.js";

// The npm version of the server this plugin is built against. Pinned, not
// floating: a server whose tool surface moved under us is worse than one that
// is a version behind. Bump deliberately.
//
// Exported from here (not declared in launch.js) so it is the SAME value the
// e2e test's devDependency is checked against in test/server-args.test.js — a
// test fails loudly if the two ever disagree, instead of the e2e suite
// silently validating a server version no user actually launches.
export const SERVER_VERSION = "0.0.6";

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

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
export const SERVER_VERSION = "0.0.10";

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
  const { scope, readScopes } = sessionScopes({ projectDir });
  return [
    "--db",
    path.join(dataDir, "memory.redb"),
    "--scope",
    scope,
    "--read-scopes",
    readScopes.join(","),
  ];
}

/**
 * THIS session's scopes — as opposed to `serverArgs`, which are the scopes baked
 * into the ONE server process the broker owns.
 *
 * Those two are not the same thing, and conflating them was the scope-bleed bug:
 * one `topodb-mcp` is shared by every concurrent session (redb allows only one
 * process to hold the database), so its `--scope` can only ever reflect whichever
 * session spawned the broker first. Every OTHER session then inherited that
 * project's scope and silently read and wrote into its memory.
 *
 * So the CLI args are now just a fallback default, and the real scope travels
 * per-request: the shim sends these to the broker on connect, and the broker
 * stamps them into each forwarded request's `_meta`. See `broker.js`'s HELLO
 * handling and `TopoServer::for_request` in `crates/topodb-mcp/src/server.rs`.
 */
export function sessionScopes({ projectDir }) {
  const scope = projectScopeId(projectDir);
  return { scope, readScopes: [scope, "shared"] };
}

/** The one place the pinned topodb-mcp tool count lives. An exact pin on
 * purpose (same philosophy as the repo's pin-vs-crate guard): a server
 * upgrade that adds or removes tools must go red HERE, deliberately, not
 * drift silently — bump it alongside the `@topodb/topodb-mcp` dependency.
 *
 * Tracks the PUBLISHED `@topodb/topodb-mcp` pinned in package.json (0.1.1 =
 * 33 tools, including `onboarding_pointer` and `graph_snapshot`). Bump this alongside the
 * `@topodb/topodb-mcp` dependency whenever the server's tool set changes. */
export const EXPECTED_TOOL_COUNT = 33;

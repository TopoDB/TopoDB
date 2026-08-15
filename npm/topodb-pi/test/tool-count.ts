/** The one place the pinned topodb-mcp tool count lives. An exact pin on
 * purpose (same philosophy as the repo's pin-vs-crate guard): a server
 * upgrade that adds or removes tools must go red HERE, deliberately, not
 * drift silently — bump it alongside the `@topodb/topodb-mcp` dependency. */
export const EXPECTED_TOOL_COUNT = 32;

/** The one place the pinned topodb-mcp tool count lives. An exact pin on
 * purpose (same philosophy as the repo's pin-vs-crate guard): a server
 * upgrade that adds or removes tools must go red HERE, deliberately, not
 * drift silently — bump it alongside the `@topodb/topodb-mcp` dependency.
 *
 * Tracks the PUBLISHED `@topodb/topodb-mcp` pinned in package.json (0.0.17 =
 * 31 tools). The onboarding work adds a 32nd tool (`onboarding_pointer`) to
 * the server source, but it is not published yet — bump this to 32 in the
 * same change that republishes `@topodb/topodb-mcp` and bumps the pin here. */
export const EXPECTED_TOOL_COUNT = 31;

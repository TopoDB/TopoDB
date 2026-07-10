# Pi SDK verification findings (Task 1 de-risk spike)

Verified 2026-07-10 by hands-on `npm view` + `npm install` probe in a scratch
dir outside this repo (`%TEMP%/pi-sdk-probe`), followed by grepping the
package's own shipped `.d.ts` files. Nothing here is guessed — every claim
below quotes or cites a real file from the installed package.

## 1. Package names/versions — CONFIRMED REAL

Both candidate package names from the docs turned out to be correct. `npm view`
output (verbatim):

```
$ npm view @earendil-works/pi-coding-agent version dist-tags
version = '0.80.6'
dist-tags = { 'legacy-node20': '0.74.2', latest: '0.80.6' }

$ npm view @earendil-works/pi-ai version
0.80.6
```

**Pin**: `@earendil-works/pi-coding-agent@0.80.6` and `@earendil-works/pi-ai@0.80.6`
(same version train, keep them in lockstep). Note the `legacy-node20` dist-tag
(`0.74.2`) — only relevant if the target runtime is Node 20; this repo's `node
--version` during the probe was v24.12.0, so `latest` (0.80.6) applies and
that's what got installed.

`package.json` `engines` for `pi-coding-agent`: `"node": ">=22.19.0"`.

## 2. Module system — ESM, confirmed from package.json

`@earendil-works/pi-coding-agent/package.json`:
```json
"type": "module",
"main": "./dist/index.js",
"types": "./dist/index.d.ts",
"exports": {
  ".": { "types": "./dist/index.d.ts", "import": "./dist/index.js" },
  "./rpc-entry": { "import": "./dist/rpc-entry.js" }
}
```
No `"require"` condition in `exports` — this package is ESM-only. Same for
`@earendil-works/pi-ai/package.json` (`"type": "module"`, `"main":
"./dist/index.js"`, `"types": "./dist/index.d.ts"`).

**Implication for `npm/topodb-pi`**: the extension entrypoint must be authored/emitted
as ESM (`.ts` compiled to ESM, or `.mts`), and package.json for `topodb-pi` should
set `"type": "module"` (or use `.mjs`/`.mts`) to interop cleanly.

## 3. `ExtensionAPI.registerTool` — verified from `dist/core/extensions/types.d.ts`

Source: `node_modules/@earendil-works/pi-coding-agent/dist/core/extensions/types.d.ts:874`

```ts
export interface ExtensionAPI {
  ...
  /** Register a tool that the LLM can call. */
  registerTool<TParams extends TSchema = TSchema, TDetails = unknown, TState = any>(
    tool: ToolDefinition<TParams, TDetails, TState>
  ): void;
  ...
}
```

The extension entrypoint itself is a default-exported factory function taking
`ExtensionAPI`:
```ts
export type ExtensionFactory = (pi: ExtensionAPI) => void | Promise<void>;
```
confirmed both in the type (`types.d.ts:1060`) and in every real example
(e.g. `examples/extensions/hello.ts`):
```ts
export default function (pi: ExtensionAPI) {
  pi.registerTool(helloTool);
}
```

## 4. `ToolDefinition` field names — verified from `dist/core/extensions/types.d.ts:335`

```ts
/**
 * Tool definition for registerTool().
 */
export interface ToolDefinition<TParams extends TSchema = TSchema, TDetails = unknown, TState = any> {
  /** Tool name (used in LLM tool calls) */
  name: string;
  /** Human-readable label for UI */
  label: string;
  /** Description for LLM */
  description: string;
  /** Optional one-line snippet for the Available tools section in the default system prompt. Custom tools are omitted from that section when this is not provided. */
  promptSnippet?: string;
  /** Optional guideline bullets appended to the default system prompt Guidelines section when this tool is active. */
  promptGuidelines?: string[];
  /** Parameter schema (TypeBox) */
  parameters: TParams;
  /** Controls whether ToolExecutionComponent renders the standard colored shell or the tool renders its own framing. */
  renderShell?: "default" | "self";
  /** Optional compatibility shim to prepare raw tool call arguments before schema validation. Must return an object conforming to TParams. */
  prepareArguments?: (args: unknown) => Static<TParams>;
  /** Per-tool execution mode override: "sequential" | "parallel". If omitted, the default execution mode applies. */
  executionMode?: ToolExecutionMode;
  /** Execute the tool. */
  execute(toolCallId: string, params: Static<TParams>, signal: AbortSignal | undefined,
          onUpdate: AgentToolUpdateCallback<TDetails> | undefined, ctx: ExtensionContext): Promise<AgentToolResult<TDetails>>;
  /** Custom rendering for tool call display */
  renderCall?: (args: Static<TParams>, theme: Theme, context: ToolRenderContext<TState, Static<TParams>>) => Component;
  /** Custom rendering for tool result display */
  renderResult?: (result: AgentToolResult<TDetails>, options: ToolRenderResultOptions, theme: Theme, context: ToolRenderContext<TState, Static<TParams>>) => Component;
}
```

There is also a `defineTool()` helper (same file, line 375) purely for type
inference when a tool is built as a standalone variable before being passed
to `registerTool`:
```ts
export declare function defineTool<TParams extends TSchema, TDetails = unknown, TState = any>(
  tool: ToolDefinition<TParams, TDetails, TState>
): ToolDefinition<TParams, TDetails, TState> & AnyToolDefinition;
```
Used like: `const helloTool = defineTool({ ... }); pi.registerTool(helloTool);`
(exactly as in `examples/extensions/hello.ts`).

## 5. `execute(...)` parameter list and return type — verified

**Exact signature** (from `ToolDefinition.execute`, `types.d.ts:361`):
```ts
execute(
  toolCallId: string,
  params: Static<TParams>,
  signal: AbortSignal | undefined,
  onUpdate: AgentToolUpdateCallback<TDetails> | undefined,
  ctx: ExtensionContext,
): Promise<AgentToolResult<TDetails>>
```

Parameter order is: **`toolCallId, params, signal, onUpdate, ctx`**. Confirmed
against real usage in `examples/extensions/hello.ts`:
```ts
async execute(_toolCallId, params, _signal, _onUpdate, _ctx) { ... }
```

⚠️ **Discrepancy found and resolved in favor of the `.d.ts`/real code**: the
shipped `docs/extensions.md` "Quick Start" snippet shows a *different* (wrong)
order — `execute(toolCallId, params, onUpdate, ctx, signal)`. That doc snippet
is stale/inconsistent with the actual type declaration and with every working
example file. Per the task brief's own tie-break rule, later tasks MUST use
the `.d.ts`-verified order (`toolCallId, params, signal, onUpdate, ctx`), not
the doc prose.

**Return type**: `Promise<AgentToolResult<TDetails>>`, where (from
`@earendil-works/pi-agent-core/dist/types.d.ts`, re-exported by
`pi-coding-agent`'s `dist/index.d.ts`):
```ts
/** Final or partial result produced by a tool. */
export interface AgentToolResult<T> {
  /** Text or image content returned to the model. */
  content: (TextContent | ImageContent)[];
  /** Arbitrary structured details for logs or UI rendering. */
  details: T;
  /** Hint that the agent should stop after the current tool batch. Early termination
   * only happens when every finalized tool result in the batch sets this to true. */
  terminate?: boolean;
}
```
`content` entries observed in practice as `{ type: "text", text: string }`.

`AgentToolUpdateCallback` (same file):
```ts
export type AgentToolUpdateCallback<T = any> = (partialResult: AgentToolResult<T>) => void;
```

Both `AgentToolResult` and `AgentToolUpdateCallback` are re-exported directly
from `@earendil-works/pi-coding-agent`'s top-level entrypoint (`dist/index.d.ts`
does `export type { ..., AgentToolResult, AgentToolUpdateCallback, ... } from
"./core/extensions/index.ts"`), so `npm/topodb-pi` only needs a dependency on
`@earendil-works/pi-coding-agent` to import these types — no direct dependency
on `@earendil-works/pi-agent-core` is required (it's a transitive dep, nested
under `pi-coding-agent`'s own `node_modules` in the probe install).

## 6. Enum helper — `StringEnum`, confirmed real, import path `@earendil-works/pi-ai`

Source: `node_modules/@earendil-works/pi-ai/dist/utils/typebox-helpers.d.ts`:
```ts
/**
 * Creates a string enum schema compatible with Google's API and other providers
 * that don't support anyOf/const patterns.
 *
 * @example
 * const OperationSchema = StringEnum(["add", "subtract", "multiply", "divide"], {
 *   description: "The operation to perform"
 * });
 *
 * type Operation = Static<typeof OperationSchema>; // "add" | "subtract" | "multiply" | "divide"
 */
export declare function StringEnum<T extends readonly string[]>(values: T, options?: {
    description?: string;
    default?: T[number];
}): TUnsafe<T[number]>;
```

Import path confirmed both by the package's own README guidance
(`examples/extensions/README.md`) and by real extension code:
```ts
import { StringEnum } from "@earendil-works/pi-ai";
...
action: StringEnum(["list", "add", "toggle", "clear"] as const),
```
(seen verbatim in `examples/extensions/todo.ts`, `tic-tac-toe.ts`,
`subagent/index.ts`).

**This is REQUIRED, not optional style**, per the shipped guidance:
> **Use StringEnum for string parameters** (required for Google API
> compatibility): `Type.Union([Type.Literal(...), ...])` does **not** work
> with Google's API. `topodb-pi` tool params with a closed string set (e.g.
> an `op` field) must use `StringEnum`, not `Type.Union`/`Type.Literal`.

`Type` itself (TypeBox's schema builder, e.g. `Type.Object`, `Type.String`,
`Type.Array`, `Type.Optional`) is re-exported by `@earendil-works/pi-ai`
(`dist/index.d.ts:2`: `export { Type } from "typebox";`) — importable from
either `@earendil-works/pi-ai` (as in `hello.ts`) or directly from `typebox`
(as in `questionnaire.ts`); both resolve to the same TypeBox package since
`pi-ai` merely re-exports it. Prefer importing `Type` and `StringEnum` both
from `@earendil-works/pi-ai` for consistency, since `typebox` itself is not a
declared direct dependency of `topodb-pi` in the plan.

## 7. Other relevant confirmed facts

- Extensions are auto-discovered from `~/.pi/agent/extensions/` (global) or
  `.pi/extensions/` (project-local); `pi -e ./path.ts` is for quick manual
  tests only (`docs/extensions.md`).
- `ExtensionContext` (5th `execute` arg) exposes `ctx.mode`, `ctx.ui.*`
  (`notify`, `confirm`, `select`, `input`, `custom`), and `ctx.sessionManager`
  — used for TUI interaction and session state, not required for a
  headless MCP-bridging tool but available if needed.
- `pi.registerCommand(name, options)` and `pi.on(eventName, handler)` are
  separate `ExtensionAPI` methods, sibling to `registerTool`, both confirmed
  in `types.d.ts:839-919`.

## Verdict

The SDK is real, installable, and its API surface matches the plan's
assumptions with one correction: **`execute`'s parameter order is
`(toolCallId, params, signal, onUpdate, ctx)`**, not the order shown in the
package's own `docs/extensions.md` quick-start snippet. Later tasks writing
`extension.ts` must follow the signatures in this file (verified against
`.d.ts` + real example code), not the docs/extensions.md prose example.

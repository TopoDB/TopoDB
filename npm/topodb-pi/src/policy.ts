// src/policy.ts — optional PolicyVersion bootstrap: records which prompt/
// config artifacts were in effect for a run. Hashes the configured artifact
// files and find-or-creates the content-addressed Artifact nodes plus the
// PolicyVersion node that INCLUDES exactly that set, so later analysis can
// tell which policy version produced which episodes.
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";

export interface HashedArtifact {
  path: string;
  sha256: string;
  content: string; // inline if <= 64 KiB else ""
  kind: string;
}

const INLINE_LIMIT = 64 * 1024;

export function hashArtifacts(paths: string[]): HashedArtifact[] {
  return [...paths]
    .sort()
    .map((p) => {
      const bytes = readFileSync(p);
      return {
        path: p.replace(/\\/g, "/"),
        sha256: createHash("sha256").update(bytes).digest("hex"),
        content: bytes.length <= INLINE_LIMIT ? bytes.toString("utf8") : "",
        kind: p.endsWith(".md") ? "prompt" : "config",
      };
    });
}

type Call = (tool: string, args: Record<string, unknown>) => Promise<unknown>;

/** Find-or-create the PolicyVersion for exactly this artifact set. Returns
 * its node id, or undefined on ANY failure (recording must never break the
 * agent). Wire protocol shapes follow topodb-mcp's JSON results; every
 * access is defensive. */
export async function ensurePolicyVersion(
  call: Call,
  paths: string[],
): Promise<string | undefined> {
  try {
    // Dedupe by sha256 (keep the first path per hash — hashArtifacts sorts
    // by path). Everything downstream — the wanted set, the reuse gate, the
    // create batch, the INCLUDES links — is driven off this deduped list;
    // duplicate content must map to ONE Artifact node and ONE edge, or the
    // exact-match reuse check breaks forever after.
    const bySha = new Map<string, HashedArtifact>();
    for (const a of hashArtifacts(paths)) {
      if (!bySha.has(a.sha256)) bySha.set(a.sha256, a);
    }
    const arts = [...bySha.values()];
    if (arts.length === 0) return undefined;
    const wanted = new Set(arts.map((a) => a.sha256));

    // 1. Resolve each artifact node by sha256 (find_by_prop).
    const artIds = new Map<string, string>(); // sha256 -> node id
    for (const a of arts) {
      const res = (await call("find_by_prop", {
        label: "Artifact",
        prop: "sha256",
        value: a.sha256,
        scope: "shared",
      })) as { nodes?: Array<{ id?: string }> };
      const id = res?.nodes?.[0]?.id;
      if (id) artIds.set(a.sha256, id);
    }

    // 2. If every artifact exists, look for a PolicyVersion whose INCLUDES
    //    set matches exactly (traverse inbound from any one artifact).
    if (artIds.size === arts.length) {
      const anyArt = [...artIds.values()][0];
      const res = (await call("traverse", {
        seed_id: anyArt,
        max_hops: 2,
        direction: "both",
        edge_types: ["INCLUDES"],
        scope: "shared",
      })) as {
        subgraph?: {
          nodes?: Array<{ id: string; label?: string; props?: Record<string, unknown> }>;
          edges?: Array<{ from: string; to: string }>;
        };
      };
      const nodes = res?.subgraph?.nodes ?? [];
      const edges = res?.subgraph?.edges ?? [];
      const shaById = new Map(
        nodes
          .filter((n) => n.label === "Artifact")
          .map((n) => [n.id, String(n.props?.sha256 ?? "")]),
      );
      for (const v of nodes.filter((n) => n.label === "PolicyVersion")) {
        const included = edges
          .filter((e) => e.from === v.id)
          .map((e) => shaById.get(e.to))
          .filter(Boolean) as string[];
        if (
          included.length === wanted.size &&
          included.every((s) => wanted.has(s))
        ) {
          return v.id; // exact match — reuse, write nothing
        }
      }
    }

    // 3. Create what's missing in ONE batch: absent Artifacts, the new
    //    PolicyVersion, INCLUDES edges. v1 intentionally does NOT maintain a
    //    Harness node or an ACTIVE_POLICY edge pointing at "the current"
    //    version — that would need the previous version's edge closed on
    //    every rotation, which is out of scope here. Each episode instead
    //    links directly to the PolicyVersion it used (see USED_POLICY in
    //    recorder.ts), so "what was active when" is derivable per-episode
    //    without a mutable singleton.
    const cmds: unknown[] = [];
    const refOf = new Map<string, string>(); // sha256 -> "#N" or literal id
    for (const a of arts) {
      const existing = artIds.get(a.sha256);
      if (existing) {
        refOf.set(a.sha256, existing);
      } else {
        refOf.set(a.sha256, `#${cmds.length}`);
        cmds.push({
          op: "create_node",
          label: "Artifact",
          scope: "shared",
          props: { path: a.path, kind: a.kind, sha256: a.sha256, content: a.content },
        });
      }
    }
    const verRef = `#${cmds.length}`;
    cmds.push({
      op: "create_node",
      label: "PolicyVersion",
      scope: "shared",
      props: { version: Date.now(), created_at: Date.now(), note: "recorder bootstrap" },
    });
    for (const a of arts) {
      cmds.push({
        op: "link",
        from: verRef,
        to: refOf.get(a.sha256),
        type: "INCLUDES",
      });
    }
    const res = (await call("submit_batch", { commands: cmds })) as {
      ids?: Array<string | null>;
    };
    const verIdx = cmds.findIndex(
      (c) => (c as { label?: string }).label === "PolicyVersion",
    );
    return res?.ids?.[verIdx] ?? undefined;
  } catch (e) {
    console.error(`topodb recorder: policy bootstrap failed: ${(e as Error).message}`);
    return undefined;
  }
}

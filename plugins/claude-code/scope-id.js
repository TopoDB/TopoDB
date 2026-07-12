// Derives a project's topodb ScopeId from its path. No registry file, so there
// is no state to corrupt and no write race between concurrent sessions in
// different repos.
//
//   ScopeId = ULID(first 16 bytes of sha256(canonical absolute project path))
//
// The output must be byte-identical to what Rust's `ScopeId` Display produces
// for the same 128-bit value, because the server parses it with `FromStr`
// (crates/topodb/src/ids.rs:44). test/e2e.test.js proves that against the real
// server rather than trusting this comment.
import { createHash } from "node:crypto";
import { realpathSync } from "node:fs";
import { resolve } from "node:path";

// Crockford base32: no I, L, O, or U (they invite transcription errors).
const ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/** 16 bytes (big-endian u128) -> the canonical 26-char ULID string. */
export function encodeUlid(bytes) {
  if (bytes.length !== 16) {
    throw new Error(`encodeUlid expects 16 bytes, got ${bytes.length}`);
  }
  let n = 0n;
  for (const b of bytes) n = (n << 8n) | BigInt(b);
  // 26 chars * 5 bits = 130 bits for a 128-bit value: the leading char carries
  // only the top 2 bits, which is why a max ULID starts at '7' and not 'Z'.
  let out = "";
  for (let i = 25; i >= 0; i--) {
    out = ALPHABET[Number(n & 31n)] + out;
    n >>= 5n;
  }
  return out;
}

/**
 * Canonicalize so two spellings of the same directory yield one scope.
 * `realpathSync` resolves symlinks and, on Windows, restores the on-disk
 * casing. Windows paths are additionally lowercased because NTFS is
 * case-insensitive — `C:\Repo` and `c:\repo` are the same directory and must
 * not become two scopes. POSIX casing is left alone: there, they are genuinely
 * different directories.
 */
function canonical(dir) {
  let p = resolve(dir);
  try {
    p = realpathSync.native(p);
  } catch {
    // Not yet on disk (or unreadable): fall back to the resolved path rather
    // than failing to start. A wrong-but-stable scope beats no memory at all.
  }
  return process.platform === "win32" ? p.toLowerCase() : p;
}

/** The project scope for `dir`, as a ULID string the server will accept. */
export function projectScopeId(dir) {
  const digest = createHash("sha256").update(canonical(dir), "utf8").digest();
  return encodeUlid(new Uint8Array(digest.subarray(0, 16)));
}

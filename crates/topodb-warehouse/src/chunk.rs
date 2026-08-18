//! Deterministic chunkers (spec §7). Token = whitespace-separated word.
use crate::event::ArtifactType;

pub const TARGET_TOKENS: usize = 512;
pub const OVERLAP_TOKENS: usize = 64;
pub const COMMAND_BLOCK_BYTES: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub idx: u32,
    pub offset: usize,
    pub len: usize,
    pub text: String,
}

fn lines_with_offsets(text: &str) -> Vec<(usize, usize)> {
    // (offset, len) per line INCLUDING its '\n' where present
    let mut out = Vec::new();
    let mut start = 0;
    for (i, ch) in text.char_indices() {
        if ch == '\n' {
            out.push((start, i + 1 - start));
            start = i + 1;
        }
    }
    if start < text.len() {
        out.push((start, text.len() - start));
    }
    out
}
fn tokens(s: &str) -> usize {
    s.split_whitespace().count()
}

fn windows(text: &str, target: usize, overlap: usize) -> Vec<Chunk> {
    let lines = lines_with_offsets(text);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let mut j = i;
        let mut toks = 0usize;
        while j < lines.len() {
            let t = tokens(&text[lines[j].0..lines[j].0 + lines[j].1]);
            if toks > 0 && toks + t > target {
                break;
            }
            toks += t;
            j += 1;
        }
        let (off, end) = (lines[i].0, lines[j - 1].0 + lines[j - 1].1);
        out.push(Chunk {
            idx: out.len() as u32,
            offset: off,
            len: end - off,
            text: text[off..end].to_string(),
        });
        if j >= lines.len() {
            break;
        }
        // step back `overlap` tokens' worth of lines (at least one line of progress)
        let mut back = j;
        let mut ot = 0usize;
        while back > i + 1 && ot < overlap {
            back -= 1;
            ot += tokens(&text[lines[back].0..lines[back].0 + lines[back].1]);
        }
        i = back.max(i + 1);
    }
    out
}

fn hunks(text: &str) -> Vec<Chunk> {
    let starts: Vec<usize> = text
        .match_indices("\n@@")
        .map(|(i, _)| i + 1)
        .chain(if text.starts_with("@@") {
            Some(0)
        } else {
            None
        })
        .collect();
    let mut starts = starts;
    starts.sort_unstable();
    starts.dedup();
    if starts.is_empty() {
        return windows(text, TARGET_TOKENS, OVERLAP_TOKENS);
    }
    let mut out = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let e = starts.get(k + 1).copied().unwrap_or(text.len());
        out.push(Chunk {
            idx: k as u32,
            offset: s,
            len: e - s,
            text: text[s..e].to_string(),
        });
    }
    out
}

fn blocks(text: &str, max_bytes: usize) -> Vec<Chunk> {
    let lines = lines_with_offsets(text);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let off = lines[i].0;
        let mut end = off;
        let mut j = i;
        while j < lines.len() && (end == off || end - off + lines[j].1 <= max_bytes) {
            end = lines[j].0 + lines[j].1;
            j += 1;
        }
        out.push(Chunk {
            idx: out.len() as u32,
            offset: off,
            len: end - off,
            text: text[off..end].to_string(),
        });
        i = j;
    }
    out
}

pub fn chunk_artifact(kind: &ArtifactType, text: &str) -> Vec<Chunk> {
    if text.is_empty() {
        return Vec::new();
    }
    match kind {
        ArtifactType::Diff => hunks(text),
        ArtifactType::Command => blocks(text, COMMAND_BLOCK_BYTES),
        _ => windows(text, TARGET_TOKENS, OVERLAP_TOKENS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::ArtifactType;

    #[test]
    fn short_text_is_one_chunk_with_valid_offsets() {
        let t = "fn main() {}\nfn other() {}\n";
        let c = chunk_artifact(&ArtifactType::FileRead, t);
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].idx, c[0].offset, c[0].len), (0, 0, t.len()));
        assert_eq!(&t[c[0].offset..c[0].offset + c[0].len], c[0].text);
    }

    #[test]
    fn long_file_is_line_aligned_windows_with_overlap_and_deterministic() {
        let t: String = (0..2000)
            .map(|i| format!("line number {i} with some words here\n"))
            .collect();
        let a = chunk_artifact(&ArtifactType::FileRead, &t);
        let b = chunk_artifact(&ArtifactType::FileRead, &t);
        assert_eq!(a, b);
        assert!(a.len() > 5);
        for c in &a {
            assert_eq!(&t[c.offset..c.offset + c.len], c.text);
            assert!(c.text.ends_with('\n') || c.offset + c.len == t.len());
        }
        // consecutive windows overlap (later starts before earlier ends)
        assert!(a[1].offset < a[0].offset + a[0].len);
        assert!(a.iter().enumerate().all(|(i, c)| c.idx as usize == i));
    }

    #[test]
    fn diff_splits_per_hunk() {
        let d = "--- a\n+++ b\n@@ -1,2 +1,2 @@\n-x\n+y\n@@ -10,2 +10,2 @@\n-p\n+q\n";
        let c = chunk_artifact(&ArtifactType::Diff, d);
        assert_eq!(c.len(), 2);
        assert!(c[0].text.starts_with("@@ -1,2"));
        assert!(c[1].text.starts_with("@@ -10,2"));
        assert_eq!(&d[c[1].offset..c[1].offset + c[1].len], c[1].text);
    }

    #[test]
    fn command_output_blocks_at_line_boundaries() {
        let t: String = (0..500).map(|i| format!("out {i}\n")).collect();
        let c = chunk_artifact(&ArtifactType::Command, &t);
        assert!(c.len() >= 2);
        for w in c.windows(2) {
            assert_eq!(w[0].offset + w[0].len, w[1].offset);
        } // no overlap for commands
        assert!(c.iter().all(|c| c.len <= COMMAND_BLOCK_BYTES + 64));
    }

    #[test]
    fn empty_text_yields_no_chunks() {
        assert!(chunk_artifact(&ArtifactType::ToolOutput, "").is_empty());
    }
}

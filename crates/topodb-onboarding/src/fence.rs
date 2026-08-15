const START_PREFIX: &str = "<!-- topodb:pointer:start version=";
const END_MARKER: &str = "<!-- topodb:pointer:end -->";

#[derive(Debug, PartialEq)]
pub enum FenceOutcome {
    Injected,
    Replaced,
    Skipped,
    Unchanged,
}

pub fn upsert_fence(existing: &str, block: &str, version: u32) -> (String, FenceOutcome) {
    let start = existing.find(START_PREFIX);
    let end = existing.find(END_MARKER);
    match (start, end) {
        (None, None) => {
            let sep = if existing.is_empty() || existing.ends_with("\n\n") {
                ""
            } else if existing.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            (format!("{existing}{sep}{block}"), FenceOutcome::Injected)
        }
        (Some(s), Some(e)) if e > s => {
            // parse existing version from the start marker line
            let after = &existing[s + START_PREFIX.len()..];
            let existing_v: u32 = after
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .unwrap_or("")
                .parse()
                .unwrap_or(0);
            if existing_v >= version {
                return (existing.to_string(), FenceOutcome::Unchanged);
            }
            let end_full = e + END_MARKER.len();
            // include a trailing newline if the block carries one
            let tail = existing[end_full..]
                .strip_prefix('\n')
                .map(|_| &existing[end_full + 1..])
                .unwrap_or(&existing[end_full..]);
            let mut out = String::new();
            out.push_str(&existing[..s]);
            // Caller-supplied block is always terminated by exactly one '\n' (from content::pointer_block), so safe to strip.
            out.push_str(block.trim_end_matches('\n'));
            out.push('\n');
            out.push_str(tail);
            (out, FenceOutcome::Replaced)
        }
        _ => (existing.to_string(), FenceOutcome::Skipped), // exactly one marker → corrupted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const START: &str = "<!-- topodb:pointer:start version=";
    const END: &str = "<!-- topodb:pointer:end -->";

    fn block(v: u32) -> String {
        format!("{START}{v} -->\nBODY v{v}\n{END}\n")
    }

    #[test]
    fn appends_when_absent() {
        let (out, o) = upsert_fence("# My rules\n", &block(1), 1);
        assert!(matches!(o, FenceOutcome::Injected));
        assert!(out.starts_with("# My rules\n"));
        assert!(out.contains(&block(1)));
    }
    #[test]
    fn replaces_in_place_on_newer_version() {
        let existing = format!("top\n{}\nbottom\n", block(1).trim_end());
        let (out, o) = upsert_fence(&existing, &block(2), 2);
        assert!(matches!(o, FenceOutcome::Replaced));
        assert!(out.contains("BODY v2"));
        assert!(!out.contains("BODY v1"));
        assert!(out.contains("top\n") && out.contains("bottom\n"));
    }
    #[test]
    fn unchanged_when_same_or_newer_version_present() {
        let existing = format!("x\n{}\n", block(2).trim_end());
        let (out, o) = upsert_fence(&existing, &block(2), 2);
        assert!(matches!(o, FenceOutcome::Unchanged));
        assert_eq!(out, existing);
    }
    #[test]
    fn skips_on_corrupted_single_marker() {
        let existing = format!("x\n{START}1 -->\nno end marker here\n");
        let (out, o) = upsert_fence(&existing, &block(1), 1);
        assert!(matches!(o, FenceOutcome::Skipped));
        assert_eq!(out, existing);
    }
    #[test]
    fn malformed_version_parses_as_zero_and_gets_replaced() {
        // Version string "abc" is non-numeric, so it parses as 0 (older than any version >= 1)
        let existing = format!("top\n{START}abc -->\nGARBLED\n{END}\nbottom\n");
        let (out, o) = upsert_fence(&existing, &block(2), 2);
        assert!(matches!(o, FenceOutcome::Replaced));
        assert!(out.contains("BODY v2"));
        assert!(!out.contains("GARBLED"));
        assert!(out.contains("top\n") && out.contains("bottom\n"));
    }
    #[test]
    fn reversed_markers_order_is_skipped() {
        // END marker appears before START marker (both present but out of order)
        let existing = format!("{END}\n{START}1 -->\nBODY v1\n{END}\n");
        let (out, o) = upsert_fence(&existing, &block(2), 2);
        assert!(matches!(o, FenceOutcome::Skipped));
        assert_eq!(out, existing);
    }
}

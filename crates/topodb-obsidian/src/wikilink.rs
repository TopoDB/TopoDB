use std::collections::BTreeSet;
use topodb_json::entity_dedup_key;

/// Scan for `[[target]]` / `[[target|alias]]` / `[[target#heading]]`.
/// Embeds (`![[…]]`) are transclusions, not references — skipped.
pub fn extract_wikilinks(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let mut i = 0;
    while let Some(pos) = text[i..].find("[[") {
        let start = i + pos;
        let embed = start > 0 && bytes[start - 1] == b'!';
        let Some(end) = text[start + 2..].find("]]") else {
            break;
        };
        let inner = &text[start + 2..start + 2 + end];
        i = start + 2 + end + 2;
        if embed {
            continue;
        }
        let target = inner.split('|').next().unwrap_or("");
        let target = target.split('#').next().unwrap_or("").trim();
        if target.is_empty() {
            continue;
        }
        if seen.insert(entity_dedup_key(target)) {
            out.push(target.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_dedups_and_orders() {
        let links = extract_wikilinks("See [[TopoDB]] and [[redb]] then [[topodb]] again.");
        assert_eq!(links, vec!["TopoDB", "redb"]); // dedup is case-insensitive, first wins
    }

    #[test]
    fn strips_alias_and_heading() {
        assert_eq!(
            extract_wikilinks("[[ort|ONNX Runtime]] [[Design#Goals]]"),
            vec!["ort", "Design"]
        );
    }

    #[test]
    fn ignores_embeds_empties_and_unterminated() {
        assert_eq!(
            extract_wikilinks("![[image.png]] [[ ]] [[#only-head]]"),
            Vec::<String>::new()
        );
        assert_eq!(
            extract_wikilinks("open [[never closed"),
            Vec::<String>::new()
        );
    }
}

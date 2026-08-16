//! Unit: OKF markdown link extraction + resolution. OKF uses
//! `[label](/path.md)` absolute-from-root and `[label](path.md)`
//! relative-to-the-file's-directory links, NOT obsidian `[[wikilinks]]`
//! (design §"Why a new crate", point 1, and §"Body links").

use topodb_okf::{extract_links, resolve_link};

#[test]
fn extract_links_returns_label_and_href_in_order() {
    let body = "See [Customers](/tables/customers.md) and [Rev](revenue.md).\n";
    let links = extract_links(body);
    assert_eq!(
        links,
        vec![
            ("Customers".to_string(), "/tables/customers.md".to_string()),
            ("Rev".to_string(), "revenue.md".to_string()),
        ]
    );
}

#[test]
fn resolve_absolute_from_root_ignores_source_dir() {
    // A leading-slash href is bundle-root-absolute regardless of the file's dir.
    assert_eq!(
        resolve_link("metrics", "/tables/customers.md").as_deref(),
        Some("tables/customers.md")
    );
}

#[test]
fn resolve_relative_is_relative_to_source_dir() {
    assert_eq!(
        resolve_link("metrics", "revenue.md").as_deref(),
        Some("metrics/revenue.md")
    );
    assert_eq!(
        resolve_link("metrics", "../tables/customers.md").as_deref(),
        Some("tables/customers.md")
    );
}

#[test]
fn resolve_ignores_external_and_non_md_links() {
    assert_eq!(resolve_link("metrics", "https://example.com"), None);
    assert_eq!(resolve_link("metrics", "mailto:x@y.z"), None);
}

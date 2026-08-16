//! Ingest — OKF bundle → graph. Asserts the data model of design §Ingest and
//! §"Data model": one concept page → one entity (with `path` prop) + one
//! attached `about` memory + body-link `references` edges + promoted
//! provenance (actor/source entities and their edges). Broken links become
//! dangling stubs, never errors.

mod common;
use common::{
    concept_by_path, copied_bundle, has_attached_memory, okf_db, shared_lookup, PATH_PROP,
};
use topodb::{PropValue, Scope, TimeAxis};
use topodb_json::{find_existing_entity, ENTITY_LABEL, MEMORY_LABEL};
use topodb_okf::ingest_okf;

#[test]
fn ingest_creates_concepts_with_path_prop_and_attached_memory() {
    let work = tempfile::tempdir().unwrap();
    let bundle = copied_bundle(work.path());
    let db = okf_db(work.path());
    let lookup = shared_lookup();

    let r = ingest_okf(&db, &bundle, Scope::Shared, &lookup, 1, false, None).unwrap();
    // Three concept pages: index.md/log.md/_plan.md/INSTRUCTIONS.md/dotfile skipped.
    assert_eq!(r.ingested, 3, "{r:?}");
    assert!(r.errors.is_empty(), "{r:?}");

    let revenue = concept_by_path(&db, &lookup, "metrics/revenue.md");
    assert_eq!(
        revenue.props.get(PATH_PROP),
        Some(&PropValue::Str("metrics/revenue.md".into()))
    );
    assert!(
        has_attached_memory(&db, &lookup, revenue.id),
        "concept needs a memory"
    );

    // The H1-fallback page: no `title` frontmatter, name derived from `# H1`.
    let onboarding = concept_by_path(&db, &lookup, "notes/onboarding.md");
    assert!(has_attached_memory(&db, &lookup, onboarding.id));
}

#[test]
fn ingest_resolves_body_link_to_references_edge() {
    let work = tempfile::tempdir().unwrap();
    let bundle = copied_bundle(work.path());
    let db = okf_db(work.path());
    let lookup = shared_lookup();
    ingest_okf(&db, &bundle, Scope::Shared, &lookup, 1, false, None).unwrap();

    let revenue = concept_by_path(&db, &lookup, "metrics/revenue.md");
    let customers = concept_by_path(&db, &lookup, "tables/customers.md");

    // revenue.md body links to /tables/customers.md → references edge.
    let out = db
        .edges_from(&lookup, revenue.id, None, None, true, TimeAxis::Valid)
        .unwrap();
    let refs_customers = out
        .iter()
        .any(|e| e.to == customers.id && e.ty.as_str().eq_ignore_ascii_case("references"));
    assert!(
        refs_customers,
        "expected references edge revenue→customers: {out:?}"
    );
}

#[test]
fn ingest_broken_link_creates_dangling_stub_not_error() {
    let work = tempfile::tempdir().unwrap();
    let bundle = copied_bundle(work.path());
    let db = okf_db(work.path());
    let lookup = shared_lookup();
    let r = ingest_okf(&db, &bundle, Scope::Shared, &lookup, 1, false, None).unwrap();
    assert!(r.errors.is_empty(), "broken links must be tolerated: {r:?}");

    // /tables/ghost.md is not in the bundle → a dangling stub keyed by path,
    // with NO attached memory (design §"Body links": broken links are legal).
    let stub = db
        .nodes_by_prop(
            &lookup,
            ENTITY_LABEL,
            PATH_PROP,
            &PropValue::Str("tables/ghost.md".into()),
        )
        .unwrap();
    assert_eq!(stub.len(), 1, "broken link → one dangling stub");
    assert!(
        !has_attached_memory(&db, &lookup, stub[0].id),
        "a dangling stub has no memory"
    );
}

#[test]
fn ingest_promotes_provenance_to_actor_and_source_entities() {
    let work = tempfile::tempdir().unwrap();
    let bundle = copied_bundle(work.path());
    let db = okf_db(work.path());
    let lookup = shared_lookup();
    ingest_okf(&db, &bundle, Scope::Shared, &lookup, 1, false, None).unwrap();

    // generated.by / verified[].by / sources[].author become actor entities;
    // sources[].resource becomes a source entity (design §"Promoted provenance").
    let generator = find_existing_entity(&db, &lookup, "openwiki/0.3")
        .unwrap()
        .expect("generated.by actor entity");
    let reviewer = find_existing_entity(&db, &lookup, "human:drew")
        .unwrap()
        .expect("verified.by actor entity");
    let source = find_existing_entity(&db, &lookup, "https://example.com/report.pdf")
        .unwrap()
        .expect("sources[].resource source entity");
    find_existing_entity(&db, &lookup, "analyst/1.0")
        .unwrap()
        .expect("sources[].author actor entity");

    // The concept carries provenance edges to the generator, reviewer, source.
    let revenue = concept_by_path(&db, &lookup, "metrics/revenue.md");
    let out = db
        .edges_from(&lookup, revenue.id, None, None, true, TimeAxis::Valid)
        .unwrap();
    let targets: Vec<_> = out.iter().map(|e| e.to).collect();
    assert!(targets.contains(&generator.id), "generated_by edge missing");
    assert!(targets.contains(&reviewer.id), "verified_by edge missing");
    assert!(targets.contains(&source.id), "sourced_from edge missing");
}

#[test]
fn ingest_is_idempotent_on_unchanged_rerun() {
    let work = tempfile::tempdir().unwrap();
    let bundle = copied_bundle(work.path());
    let db = okf_db(work.path());
    let lookup = shared_lookup();

    let r1 = ingest_okf(&db, &bundle, Scope::Shared, &lookup, 1, false, None).unwrap();
    assert_eq!(r1.ingested, 3);
    // First run stamps `topodb-id` into the copied files.
    assert!(std::fs::read_to_string(bundle.join("metrics/revenue.md"))
        .unwrap()
        .contains("topodb-id"));

    let r2 = ingest_okf(&db, &bundle, Scope::Shared, &lookup, 2, false, None).unwrap();
    assert_eq!(
        (r2.ingested, r2.superseded, r2.deduplicated, r2.errors.len()),
        (0, 0, 0, 0),
        "unchanged re-ingest must be a pure no-op: {r2:?}"
    );
    assert!(
        r2.skipped >= 3,
        "unchanged concepts count as skipped: {r2:?}"
    );
}

#[test]
fn ingest_dry_run_plans_without_writing() {
    let work = tempfile::tempdir().unwrap();
    let bundle = copied_bundle(work.path());
    let db = okf_db(work.path());
    let lookup = shared_lookup();

    let dry = ingest_okf(&db, &bundle, Scope::Shared, &lookup, 5, true, None).unwrap();
    assert_eq!(dry.ingested, 3, "{dry:?}");
    // No id stamped and no nodes created.
    assert!(!std::fs::read_to_string(bundle.join("metrics/revenue.md"))
        .unwrap()
        .contains("topodb-id"));
    let any = db
        .nodes_by_prop(
            &lookup,
            MEMORY_LABEL,
            topodb_json::MEMORY_CONTENT_HASH_PROP,
            &PropValue::Str("nonexistent".into()),
        )
        .unwrap();
    assert!(any.is_empty());
    // A real run afterward still ingests everything.
    let real = ingest_okf(&db, &bundle, Scope::Shared, &lookup, 6, false, None).unwrap();
    assert_eq!(real.ingested, 3);
}

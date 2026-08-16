//! `.topodb.toml` read / merge / write, including the `[schedule]` table.
//!
//! Today `.topodb.toml` is read-only elsewhere in the workspace
//! (`crates/topodb-cli/src/resolve.rs`); this module introduces the first
//! *writer*. Parsing is tolerant: unknown keys/tables are preserved rather
//! than rejected. Merging never clobbers a user-set value — it only fills in
//! keys that are absent, and always stamps `onboarding_version`.
//!
//! Note: `toml::to_string_pretty` re-serializes the parsed `toml::Table`, so
//! user comments in the existing file are not preserved across a merge. This
//! is a known, accepted limitation (see task-4 brief).

/// Which ingest layer a reingest source uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Obsidian,
    Okf,
}

impl SourceKind {
    fn parse(s: &str) -> Option<SourceKind> {
        match s {
            "obsidian" => Some(SourceKind::Obsidian),
            "okf" => Some(SourceKind::Okf),
            _ => None,
        }
    }
}

/// One `[[reingest.source]]` entry: a vault/bundle to re-ingest on schedule.
/// `path` is as-written (resolved against the config dir later); `scope`
/// overrides the catch-up write scope for this source when present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReingestSource {
    pub kind: SourceKind,
    pub path: String,
    pub scope: Option<String>,
}

/// One scheduled maintenance task's config: whether it runs, and how often.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleEntry {
    pub enabled: bool,
    pub interval_secs: u64,
}

/// The `[schedule]` table: one entry per maintenance task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    pub compact: ScheduleEntry,
    pub purge: ScheduleEntry,
    pub reingest: ScheduleEntry,
    pub lifecycle: ScheduleEntry,
}

const DAILY_SECS: u64 = 86_400;
const HOURLY_SECS: u64 = 3_600;

impl Schedule {
    /// Default schedule: compact/purge/lifecycle run daily and are enabled
    /// out of the box; reingest defaults to hourly but disabled (it only
    /// makes sense once a source is configured).
    pub fn defaults() -> Self {
        Schedule {
            compact: ScheduleEntry {
                enabled: true,
                interval_secs: DAILY_SECS,
            },
            purge: ScheduleEntry {
                enabled: true,
                interval_secs: DAILY_SECS,
            },
            reingest: ScheduleEntry {
                enabled: false,
                interval_secs: HOURLY_SECS,
            },
            lifecycle: ScheduleEntry {
                enabled: true,
                interval_secs: DAILY_SECS,
            },
        }
    }
}

/// Parsed `.topodb.toml` contents, tolerant of unknown keys.
#[derive(Debug, Clone)]
pub struct OnboardingConfig {
    pub db: Option<String>,
    pub scope: Option<String>,
    pub onboarding_version: Option<u32>,
    pub schedule: Schedule,
    pub sources: Vec<ReingestSource>,
}

/// Updates to merge into an existing `.topodb.toml`.
#[derive(Debug, Clone)]
pub struct OnboardingUpdates {
    pub db: Option<String>,
    pub scope: Option<String>,
    pub onboarding_version: u32,
    pub ensure_schedule_defaults: bool,
}

fn entry_from_table(table: Option<&toml::Table>, default: ScheduleEntry) -> ScheduleEntry {
    let enabled = table
        .and_then(|t| t.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(default.enabled);
    let interval_secs = table
        .and_then(|t| t.get("interval_secs"))
        .and_then(|v| v.as_integer())
        .map(|i| i as u64)
        .unwrap_or(default.interval_secs);
    ScheduleEntry {
        enabled,
        interval_secs,
    }
}

/// Tolerant parse: never errors, missing/malformed fields fall back to
/// defaults, and unknown keys are simply ignored (they're not represented in
/// `OnboardingConfig`, but `render_merged` preserves them separately by
/// operating on the raw `toml::Table`).
pub fn parse(toml_text: &str) -> OnboardingConfig {
    let table: toml::Table = toml::from_str(toml_text).unwrap_or_default();

    let db = table
        .get("db")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let scope = table
        .get("scope")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let onboarding_version = table
        .get("onboarding_version")
        .and_then(|v| v.as_integer())
        .map(|i| i as u32);

    let defaults = Schedule::defaults();
    let schedule_table = table.get("schedule").and_then(|v| v.as_table());
    let sub = |name: &str| {
        schedule_table
            .and_then(|t| t.get(name))
            .and_then(|v| v.as_table())
    };

    let schedule = Schedule {
        compact: entry_from_table(sub("compact"), defaults.compact),
        purge: entry_from_table(sub("purge"), defaults.purge),
        reingest: entry_from_table(sub("reingest"), defaults.reingest),
        lifecycle: entry_from_table(sub("lifecycle"), defaults.lifecycle),
    };

    let sources = table
        .get("reingest")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("source"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let t = item.as_table()?;
                    let kind = SourceKind::parse(t.get("kind")?.as_str()?)?;
                    let path = t.get("path")?.as_str()?.to_string();
                    if path.is_empty() {
                        return None;
                    }
                    let scope = t
                        .get("scope")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    Some(ReingestSource { kind, path, scope })
                })
                .collect()
        })
        .unwrap_or_default();

    OnboardingConfig {
        db,
        scope,
        onboarding_version,
        schedule,
        sources,
    }
}

fn ensure_entry_defaults(schedule_table: &mut toml::Table, name: &str, default: ScheduleEntry) {
    let entry = schedule_table
        .entry(name.to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let entry_table = match entry {
        toml::Value::Table(t) => t,
        _ => {
            *entry = toml::Value::Table(toml::Table::new());
            match entry {
                toml::Value::Table(t) => t,
                _ => unreachable!(),
            }
        }
    };
    entry_table
        .entry("enabled".to_string())
        .or_insert_with(|| toml::Value::Boolean(default.enabled));
    entry_table
        .entry("interval_secs".to_string())
        .or_insert_with(|| toml::Value::Integer(default.interval_secs as i64));
}

/// Merge `updates` into `existing_text`, returning the new file text.
///
/// Never clobbers a user-set `db`/`scope`/`[schedule].<task>` value — those
/// are only filled in when absent. `onboarding_version` is always
/// overwritten. Unknown keys/tables in `existing_text` are preserved as-is
/// (modulo `toml`'s re-serialization, which does not retain comments).
pub fn render_merged(existing_text: &str, updates: &OnboardingUpdates) -> String {
    let mut table: toml::Table = match toml::from_str(existing_text) {
        Ok(table) => table,
        // Non-empty text that fails to parse is a malformed/hand-edited file:
        // refuse to clobber it with a fresh defaults-only file.
        Err(_) if !existing_text.trim().is_empty() => return existing_text.to_string(),
        // Empty or whitespace-only: no config yet, start fresh.
        Err(_) => toml::Table::new(),
    };

    if !table.contains_key("db") {
        if let Some(db) = &updates.db {
            table.insert("db".to_string(), toml::Value::String(db.clone()));
        }
    }
    if !table.contains_key("scope") {
        if let Some(scope) = &updates.scope {
            table.insert("scope".to_string(), toml::Value::String(scope.clone()));
        }
    }

    table.insert(
        "onboarding_version".to_string(),
        toml::Value::Integer(updates.onboarding_version as i64),
    );

    if updates.ensure_schedule_defaults {
        let defaults = Schedule::defaults();
        let schedule_entry = table
            .entry("schedule".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let schedule_table = match schedule_entry {
            toml::Value::Table(t) => t,
            _ => {
                *schedule_entry = toml::Value::Table(toml::Table::new());
                match schedule_entry {
                    toml::Value::Table(t) => t,
                    _ => unreachable!(),
                }
            }
        };
        ensure_entry_defaults(schedule_table, "compact", defaults.compact);
        ensure_entry_defaults(schedule_table, "purge", defaults.purge);
        ensure_entry_defaults(schedule_table, "reingest", defaults.reingest);
        ensure_entry_defaults(schedule_table, "lifecycle", defaults.lifecycle);
    }

    toml::to_string_pretty(&table).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_absent() {
        let c = parse("");
        assert_eq!(c.schedule.compact.interval_secs, 86_400);
        assert_eq!(c.schedule.reingest.interval_secs, 3_600);
        assert!(!c.schedule.reingest.enabled);
        assert_eq!(c.onboarding_version, None);
    }

    #[test]
    fn merge_preserves_user_values_and_unknown_keys() {
        let existing = "db = \"/my/db.redb\"\nscope = \"shared\"\ncustom_key = 42\n\n[schedule.compact]\ninterval_secs = 999\n";
        let updates = OnboardingUpdates {
            db: Some("/default/db.redb".into()), // must NOT overwrite the user's db
            scope: Some("shared".into()),
            onboarding_version: 1,
            ensure_schedule_defaults: true,
        };
        let out = render_merged(existing, &updates);
        let c = parse(&out);
        assert_eq!(c.db.as_deref(), Some("/my/db.redb")); // preserved
        assert_eq!(c.schedule.compact.interval_secs, 999); // preserved
        assert_eq!(c.schedule.purge.interval_secs, 86_400); // filled default
        assert_eq!(c.onboarding_version, Some(1)); // stamped
        assert!(out.contains("custom_key")); // unknown key preserved
    }

    #[test]
    fn parses_reingest_sources_array() {
        let text = "\
[[reingest.source]]
kind = \"obsidian\"
path = \"~/notes/vault\"

[[reingest.source]]
kind = \"okf\"
path = \"./knowledge\"
scope = \"shared\"
";
        let c = parse(text);
        assert_eq!(c.sources.len(), 2);
        assert_eq!(c.sources[0].kind, SourceKind::Obsidian);
        assert_eq!(c.sources[0].path, "~/notes/vault");
        assert_eq!(c.sources[0].scope, None);
        assert_eq!(c.sources[1].kind, SourceKind::Okf);
        assert_eq!(c.sources[1].path, "./knowledge");
        assert_eq!(c.sources[1].scope.as_deref(), Some("shared"));
    }

    #[test]
    fn drops_malformed_sources_and_defaults_empty() {
        // unknown kind, empty path, and missing path are all dropped
        let text = "\
[[reingest.source]]
kind = \"nope\"
path = \"/x\"

[[reingest.source]]
kind = \"obsidian\"
path = \"\"

[[reingest.source]]
kind = \"okf\"
";
        assert!(parse(text).sources.is_empty());
        assert!(parse("").sources.is_empty());
    }

    #[test]
    fn render_merged_refuses_to_clobber_malformed_file() {
        let existing = "db = \"/my/db.redb\"\nthis is not [ valid toml\n";
        let updates = OnboardingUpdates {
            db: Some("/default/db.redb".into()),
            scope: Some("shared".into()),
            onboarding_version: 1,
            ensure_schedule_defaults: true,
        };
        let out = render_merged(existing, &updates);
        assert_eq!(out, existing);
    }
}

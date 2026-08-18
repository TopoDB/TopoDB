mod content;
pub use content::{
    conventions_markdown, ensure_conventions_file, pointer_block, pointer_body, ONBOARDING_VERSION,
};

mod fence;
pub use fence::{upsert_fence, FenceOutcome};

mod config;
pub use config::{
    parse, render_merged, resolve_warehouse, OnboardingConfig, OnboardingUpdates, Schedule,
    ScheduleEntry, WarehouseSection,
};
mod hygiene;
pub use hygiene::{due_tasks, run_catch_up, CatchUpReport, Task};

mod reingest;
pub use config::{ReingestSource, SourceKind};
pub use reingest::{
    env_home, resolve_sources, run_reingest, ReingestFileError, ReingestReport, ResolvedSource,
};

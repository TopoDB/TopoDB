mod content;
pub use content::{conventions_markdown, pointer_block, pointer_body, ONBOARDING_VERSION};

mod fence;
pub use fence::{upsert_fence, FenceOutcome};
// later tasks re-export config, hygiene modules here

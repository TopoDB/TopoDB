use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use ulid::Ulid;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub struct $name(pub Ulid);
        impl $name {
            pub fn new() -> Self { Self(Ulid::new()) }
        }
        impl Default for $name {
            fn default() -> Self { Self::new() }
        }
    };
}
id_type!(NodeId);
id_type!(EdgeId);
id_type!(ScopeId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Scope {
    Shared,
    Id(ScopeId),
}

/// The mandatory scope filter for every read. There is no unscoped read.
#[derive(Debug, Clone, Default)]
pub struct ScopeSet {
    include_shared: bool,
    ids: BTreeSet<ScopeId>,
}

impl ScopeSet {
    pub fn of(ids: &[ScopeId]) -> Self {
        Self { include_shared: false, ids: ids.iter().copied().collect() }
    }
    pub fn with_shared(mut self) -> Self {
        self.include_shared = true;
        self
    }
    pub fn contains(&self, s: Scope) -> bool {
        match s {
            Scope::Shared => self.include_shared,
            Scope::Id(id) => self.ids.contains(&id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_set_contains_shared_only_when_enabled() {
        let a = ScopeId::new();
        let set = ScopeSet::of(&[a]);
        assert!(set.contains(Scope::Id(a)));
        assert!(!set.contains(Scope::Shared));
        assert!(set.clone().with_shared().contains(Scope::Shared));
        assert!(!set.contains(Scope::Id(ScopeId::new())));
    }

    #[test]
    fn ids_roundtrip_postcard() {
        let id = NodeId::new();
        let bytes = postcard::to_allocvec(&id).unwrap();
        let back: NodeId = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(id, back);
    }
}

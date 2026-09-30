//! Host-only site state, outside the model's browser operation vocabulary.

pub(crate) mod bytes;
pub(crate) mod custody;
pub(crate) mod protocol;
pub(crate) mod redaction;
pub(crate) mod state;
pub(crate) mod storage;

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

/// Shared only with the program's private CDP connection. Custody targets
/// stay host-only even though Target.getTargets sees the same Chrome.
#[derive(Clone, Default)]
pub(crate) struct Context {
    pub(crate) values: redaction::Redaction,
    pub(crate) files: crate::native::playwright::files::StagedFiles,
    targets: Arc<RwLock<HashSet<String>>>,
}

impl Context {
    pub(crate) fn private_target(&self, target: &str) -> bool {
        self.targets
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .contains(target)
    }
    pub(crate) fn hold_target(&self, target: &str) {
        self.targets
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .insert(target.into());
    }
    pub(crate) fn release_target(&self, target: &str) {
        self.targets
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .remove(target);
    }
}

#[cfg(test)]
mod e2e_tests;

//! A temporary renderer capture changes presentation without changing the
//! document. Its owner serializes that change and publishes restoration;
//! pixel consumers bind the revision as well as the document generation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub(crate) struct Presentations {
    pub(crate) order: Arc<tokio::sync::Mutex<()>>,
    states: Mutex<HashMap<String, State>>,
}

struct State {
    generation: String,
    revision: u64,
    ready: bool,
}

impl Presentations {
    pub(crate) fn observe(&self, session: &str, generation: &str) -> Option<u64> {
        let states = self.states.lock().unwrap();
        match states
            .get(session)
            .filter(|state| state.generation == generation)
        {
            Some(state) => state.ready.then_some(state.revision),
            None => Some(0), // A new document has no prior temporary capture.
        }
    }

    /// Called only under `order`, before any capture-only mutation.
    pub(crate) fn begin(&self, session: &str, generation: &str) {
        let mut states = self.states.lock().unwrap();
        let revision = states.get(session).map_or(1, |state| state.revision + 1);
        states.insert(
            session.into(),
            State {
                generation: generation.into(),
                revision,
                ready: false,
            },
        );
    }

    /// A failed or cancelled restoration cannot make old pixels admissible.
    /// The old document's publication never marks a successor ready.
    pub(crate) fn publish(&self, session: &str, generation: &str) {
        if let Some(state) = self
            .states
            .lock()
            .unwrap()
            .get_mut(session)
            .filter(|state| state.generation == generation)
        {
            state.ready = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_identity_does_not_prove_restored_presentation() {
        let owner = Presentations::default();
        assert_eq!(owner.observe("page", "original"), Some(0));
        owner.begin("page", "original");
        assert_eq!(owner.observe("page", "original"), None);
        owner.publish("page", "other");
        assert_eq!(owner.observe("page", "original"), None);
        owner.publish("page", "original");
        assert_eq!(owner.observe("page", "original"), Some(1));
        owner.begin("page", "original");
        owner.publish("page", "original");
        assert_eq!(owner.observe("page", "original"), Some(2));
        assert_eq!(owner.observe("page", "successor"), Some(0));
    }
}

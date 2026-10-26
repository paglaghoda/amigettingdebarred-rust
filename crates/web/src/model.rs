use chrono::NaiveDate;
use debarred_domain::Plan;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Welcome,
    Setup,
    Planner,
    Account,
    Privacy,
    Terms,
    NotFound,
}

impl Page {
    pub fn from_path(path: &str) -> Self {
        match path.trim_end_matches('/') {
            "" => Self::Welcome,
            "/setup" | "/demo" | "/demo/setup" => Self::Setup,
            "/planner" | "/demo/planner" => Self::Planner,
            "/account" | "/demo/account" => Self::Account,
            "/privacy" => Self::Privacy,
            "/terms" => Self::Terms,
            _ => Self::NotFound,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RuntimeInfo {
    pub mode: String,
    pub auth_available: bool,
    pub local_password_enabled: bool,
    pub ai_enabled: bool,
    pub today: Option<NaiveDate>,
    pub support_email: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct User {
    pub user_id: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub csrf_token: String,
}

#[derive(Clone)]
pub enum AuthState {
    Checking,
    SignedOut,
    SignedIn(User),
    Demo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStatus {
    Loading,
    Saved,
    Unsaved,
    Saving,
    LocalOnly,
    Failed(String),
    Conflict,
    Expired,
    Invalid(String),
}

#[derive(Clone, Deserialize, Serialize)]
pub struct StoredPlan {
    pub plan: Plan,
    pub revision: i64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    pub plan: Plan,
    pub base_revision: i64,
    pub dirty: bool,
}

#[derive(Clone)]
pub struct DeviceDraft {
    pub key: String,
    pub label: String,
    pub current: bool,
}

#[derive(Clone)]
pub struct DraftIdentity {
    pub tab_id: String,
    pub instance_id: String,
    pub updated_ms: i64,
}

pub struct RestoredPlan {
    pub plan: Plan,
    pub revision: i64,
    pub dirty: bool,
    pub conflict: bool,
}

/// A conflicting draft retains its original base revision across page reloads.
/// Adopting the newer cloud revision here would silently authorize an overwrite.
pub fn restore_plan(stored: StoredPlan, cached: Option<Draft>) -> RestoredPlan {
    if let Some(draft) = cached {
        if draft.dirty && draft.plan != stored.plan {
            return RestoredPlan {
                conflict: draft.base_revision != stored.revision,
                plan: draft.plan,
                revision: draft.base_revision,
                dirty: true,
            };
        }
    }
    RestoredPlan {
        plan: stored.plan,
        revision: stored.revision,
        dirty: false,
        conflict: false,
    }
}

#[derive(Clone)]
pub struct ChatMessage {
    pub from_user: bool,
    pub text: String,
    pub error: bool,
}

#[derive(Clone, Default, Deserialize)]
pub struct Quota {
    pub limit: i32,
    pub used: i32,
    pub remaining: i32,
}

#[derive(Clone, Default, Deserialize)]
pub struct Quotas {
    pub helper: Quota,
    pub image: Quota,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveTicket {
    pub generation: u64,
    pub edit: u64,
    pub expected_revision: i64,
}

/// Pure coordination model: requests are serialized and stale completions cannot
/// cross an account change, explicit reload, or sandbox transition.
#[derive(Debug, Clone)]
pub struct SaveState {
    pub generation: u64,
    pub edited: u64,
    pub saved: u64,
    pub revision: i64,
    pub loaded: bool,
    pub in_flight: Option<SaveTicket>,
    pub blocked: bool,
}

impl Default for SaveState {
    fn default() -> Self {
        Self {
            generation: 1,
            edited: 0,
            saved: 0,
            revision: 0,
            loaded: false,
            in_flight: None,
            blocked: false,
        }
    }
}

impl SaveState {
    pub fn reset(&mut self) {
        let generation = self.generation + 1;
        *self = Self {
            generation,
            ..Self::default()
        };
    }

    pub fn edit(&mut self) {
        self.edited += 1;
    }
    pub fn dirty(&self) -> bool {
        self.edited != self.saved
    }

    pub fn begin(&mut self) -> Option<SaveTicket> {
        if !self.loaded || self.blocked || self.in_flight.is_some() || !self.dirty() {
            return None;
        }
        let ticket = SaveTicket {
            generation: self.generation,
            edit: self.edited,
            expected_revision: self.revision,
        };
        self.in_flight = Some(ticket);
        Some(ticket)
    }

    pub fn finish(&mut self, ticket: SaveTicket, revision: i64) -> bool {
        if self.in_flight != Some(ticket) || self.generation != ticket.generation {
            return false;
        }
        self.in_flight = None;
        if revision != ticket.expected_revision + 1 {
            self.blocked = true;
            return false;
        }
        self.revision = revision;
        self.saved = ticket.edit;
        true
    }

    pub fn fail(&mut self, ticket: SaveTicket) -> bool {
        if self.in_flight != Some(ticket) || self.generation != ticket.generation {
            return false;
        }
        self.in_flight = None;
        self.blocked = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_during_save_are_not_marked_saved() {
        let mut state = SaveState {
            loaded: true,
            ..Default::default()
        };
        state.edit();
        let first = state.begin().unwrap();
        state.edit();
        assert!(state.begin().is_none());
        assert!(state.finish(first, 1));
        assert!(state.dirty());
        let second = state.begin().unwrap();
        assert_eq!(second.expected_revision, 1);
        assert!(state.finish(second, 2));
        assert!(!state.dirty());
    }

    #[test]
    fn failure_conflict_and_initial_load_cannot_overwrite() {
        let mut state = SaveState::default();
        state.edit();
        assert!(state.begin().is_none());
        state.loaded = true;
        let ticket = state.begin().unwrap();
        assert!(state.fail(ticket));
        state.edit();
        assert!(state.begin().is_none());
        assert!(state.dirty());
    }

    #[test]
    fn account_changes_reject_stale_completions() {
        let mut state = SaveState {
            loaded: true,
            ..Default::default()
        };
        state.edit();
        let ticket = state.begin().unwrap();
        state.reset();
        assert!(!state.finish(ticket, 1));
        assert!(!state.fail(ticket));
        assert_eq!(state.revision, 0);
    }

    #[test]
    fn unexpected_revisions_block_more_writes() {
        let mut state = SaveState {
            loaded: true,
            ..Default::default()
        };
        state.edit();
        let ticket = state.begin().unwrap();
        assert!(!state.finish(ticket, 8));
        assert!(state.blocked && state.dirty());
    }

    #[test]
    fn conflict_survives_repeated_reload_without_adopting_cloud_revision() {
        let mut cloud = crate::state::sample_plan();
        let local = cloud.clone();
        cloud.leave.clear();
        let first = restore_plan(
            StoredPlan {
                plan: cloud.clone(),
                revision: 4,
            },
            Some(Draft {
                plan: local,
                base_revision: 2,
                dirty: true,
            }),
        );
        assert!(first.conflict);
        assert_eq!(first.revision, 2);
        let second = restore_plan(
            StoredPlan {
                plan: cloud,
                revision: 4,
            },
            Some(Draft {
                plan: first.plan,
                base_revision: first.revision,
                dirty: first.dirty,
            }),
        );
        assert!(second.conflict);
        assert_eq!(second.revision, 2);
    }

    #[test]
    fn lost_success_response_can_be_confirmed_by_matching_cloud_plan() {
        let plan = crate::state::sample_plan();
        let result = restore_plan(
            StoredPlan {
                plan: plan.clone(),
                revision: 1,
            },
            Some(Draft {
                plan,
                base_revision: 0,
                dirty: true,
            }),
        );
        assert!(!result.conflict && !result.dirty);
        assert_eq!(result.revision, 1);
    }

    #[test]
    fn routing_does_not_accept_arbitrary_paths() {
        assert_eq!(Page::from_path("/demo/planner"), Page::Planner);
        assert_eq!(Page::from_path("/setup/"), Page::Setup);
        assert_eq!(Page::from_path("/api/me"), Page::NotFound);
    }
}

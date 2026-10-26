use chrono::NaiveDate;
use debarred_domain::{
    AttendanceSnapshot, Calendar, CheckpointProjection, Course, Engine, GridDay, LeaveEntry,
    LeaveSummary, Meeting, Plan, SkipAllowance, SlotIndex, Timetable,
};
use leptos::prelude::*;

use crate::model::*;

#[derive(Clone, Copy)]
pub struct UiContext {
    pub plan: RwSignal<Plan>,
    pub page: RwSignal<Page>,
    pub auth: RwSignal<AuthState>,
    pub sync: RwSignal<SyncStatus>,
    pub runtime: RwSignal<RuntimeInfo>,
    pub notice: RwSignal<Option<String>>,
    pub helper_open: RwSignal<bool>,
    pub helper_busy: RwSignal<bool>,
    pub messages: RwSignal<Vec<ChatMessage>>,
    pub import_busy: RwSignal<bool>,
    pub import_draft: RwSignal<Option<Timetable>>,
    pub quotas: RwSignal<Option<Quotas>>,
    pub device_drafts: RwSignal<Vec<DeviceDraft>>,
    pub account_busy: RwSignal<bool>,
    pub deletion_pending: RwSignal<bool>,
    pub(crate) calendar: StoredValue<Calendar>,
    pub(crate) saves: RwSignal<SaveState>,
    #[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
    pub(crate) identity: StoredValue<Option<DraftIdentity>>,
}

impl UiContext {
    pub fn new(path: &str) -> Self {
        let calendar = Calendar::winter_2026().expect("bundled calendar is tested");
        let demo = path == "/demo" || path.starts_with("/demo/");
        let plan = if demo {
            sample_plan()
        } else {
            empty_plan(&calendar)
        };
        Self {
            plan: RwSignal::new(plan),
            page: RwSignal::new(Page::from_path(path)),
            auth: RwSignal::new(if demo {
                AuthState::Demo
            } else {
                AuthState::Checking
            }),
            sync: RwSignal::new(SyncStatus::Loading),
            runtime: RwSignal::new(RuntimeInfo {
                mode: "loading".into(),
                ..Default::default()
            }),
            notice: RwSignal::new(None),
            helper_open: RwSignal::new(false),
            helper_busy: RwSignal::new(false),
            messages: RwSignal::new(Vec::new()),
            import_busy: RwSignal::new(false),
            import_draft: RwSignal::new(None),
            quotas: RwSignal::new(None),
            device_drafts: RwSignal::new(Vec::new()),
            account_busy: RwSignal::new(false),
            deletion_pending: RwSignal::new(false),
            calendar: StoredValue::new(calendar),
            saves: RwSignal::new(SaveState::default()),
            #[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
            identity: StoredValue::new(None),
        }
    }

    pub fn export_account_data(self) {
        #[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
        crate::browser::export_account_data(self);
    }

    pub fn request_account_deletion(self, confirmation: String) {
        #[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
        crate::browser::request_account_deletion(self, confirmation);
        #[cfg(not(all(feature = "hydrate", target_arch = "wasm32")))]
        let _ = confirmation;
    }

    pub fn boot(self) {
        #[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
        crate::browser::boot(self);
    }

    pub fn calendar(self) -> Calendar {
        self.calendar.get_value()
    }
    pub fn is_demo(self) -> bool {
        matches!(self.auth.get(), AuthState::Demo)
    }
    pub fn can_edit(self) -> bool {
        matches!(self.auth.get(), AuthState::Demo | AuthState::SignedIn(_))
            && self.saves.get().loaded
            && !matches!(self.sync.get(), SyncStatus::Expired)
    }

    pub fn validation_error(self) -> Option<String> {
        Engine::new(&self.calendar(), &self.plan.get())
            .err()
            .map(|e| e.to_string())
    }

    pub fn results(self) -> Vec<CheckpointProjection> {
        let calendar = self.calendar();
        let plan = self.plan.get();
        Engine::new(&calendar, &plan)
            .map(|engine| engine.project_all())
            .unwrap_or_default()
    }

    pub fn allowance(self, course: &str, checkpoint: &str) -> Option<SkipAllowance> {
        let calendar = self.calendar();
        let plan = self.plan.get();
        Engine::new(&calendar, &plan)
            .ok()?
            .skip_allowance(course, checkpoint)
            .ok()
    }

    pub fn held_for(self, course: &str) -> u32 {
        let calendar = self.calendar();
        let plan = counting_plan(self.plan.get());
        Engine::new(&calendar, &plan)
            .ok()
            .and_then(|e| e.held_so_far(course).ok())
            .unwrap_or(0)
    }

    pub fn meetings(self, date: NaiveDate) -> Vec<Meeting> {
        let calendar = self.calendar();
        let plan = counting_plan(self.plan.get());
        Engine::new(&calendar, &plan)
            .map(|e| e.meetings_on(date).to_vec())
            .unwrap_or_default()
    }

    pub fn leave_summary(self) -> LeaveSummary {
        let calendar = self.calendar();
        let plan = self.plan.get();
        Engine::new(&calendar, &plan)
            .map(|e| e.leave_summary())
            .unwrap_or_default()
    }

    pub fn day_selection(self, date: NaiveDate) -> (usize, usize) {
        let plan = self.plan.get();
        let meetings = self.meetings(date);
        let selected = meetings
            .iter()
            .filter(|meeting| {
                plan.leave.iter().any(|entry| match entry {
                    LeaveEntry::Day { date: d } => *d == date,
                    LeaveEntry::Slot { date: d, slot } => *d == date && *slot == meeting.slot,
                })
            })
            .count();
        (selected, meetings.len())
    }

    pub fn edit(self, change: impl FnOnce(&mut Plan)) {
        if !self.can_edit() {
            return;
        }
        self.plan.update(change);
        self.saves.update(SaveState::edit);
        if !self.saves.get_untracked().blocked {
            if let Some(error) = self.validation_error() {
                self.sync.set(SyncStatus::Invalid(error));
            } else if self.is_demo() {
                self.sync.set(SyncStatus::LocalOnly);
            } else {
                self.sync.set(SyncStatus::Unsaved);
            }
        }
        #[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
        {
            crate::drafts::touch(self);
            crate::browser::persist(self);
            crate::browser::schedule_save(self);
        }
    }

    pub fn navigate(self, page: Page) {
        self.page.set(page);
        self.helper_open.set(false);
        #[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
        crate::browser::navigate(self, page);
    }

    pub fn set_cell(self, day: GridDay, index: usize, value: String) {
        if index >= 7 {
            return;
        }
        self.edit(|plan| set_cell(plan, day, index, value));
    }

    pub fn set_course_name(self, code: String, name: String) {
        self.edit(|plan| {
            if let Some(course) = plan.timetable.courses.iter_mut().find(|c| c.code == code) {
                course.name = name;
            }
        });
    }

    pub fn remove_course(self, code: String) {
        let calendar = self.calendar();
        self.edit(|plan| {
            plan.timetable.courses.retain(|course| course.code != code);
            plan.attendance.attended.remove(&code);
            for cell in plan.timetable.grid.values_mut().flatten() {
                if cell.as_ref() == Some(&code) {
                    *cell = None;
                }
            }
            remove_obsolete_leave(&calendar, plan);
        });
    }

    pub fn set_attended(self, code: String, value: u32) {
        // Preserve invalid edits visibly for correction; never silently cap factual counts.
        self.edit(|plan| {
            plan.attendance.attended.insert(code, value);
        });
    }

    pub fn set_snapshot(self, date: NaiveDate) {
        self.edit(|plan| plan.attendance.as_of = date);
    }

    pub fn toggle_day(self, date: NaiveDate) {
        let (selected, total) = self.day_selection(date);
        if total == 0 || date <= self.plan.get_untracked().attendance.as_of {
            return;
        }
        self.edit(|plan| {
            plan.leave.retain(|entry| entry.date() != date);
            if selected == 0 {
                plan.leave.push(LeaveEntry::Day { date });
            }
        });
    }

    pub fn toggle_slot(self, date: NaiveDate, slot: SlotIndex) {
        let meetings = self.meetings(date);
        if date <= self.plan.get_untracked().attendance.as_of
            || !meetings.iter().any(|m| m.slot == slot)
        {
            return;
        }
        self.edit(|plan| toggle_slot(plan, date, slot, &meetings));
    }

    pub fn clear_leave(self) {
        self.edit(|plan| plan.leave.clear());
    }
    pub fn clear_timetable(self) {
        self.edit(|plan| {
            plan.timetable = Timetable::default();
            plan.attendance.attended.clear();
            plan.leave.clear();
        });
    }

    pub fn load_sample(self) {
        self.edit(|plan| *plan = sample_plan());
        self.notice.set(Some("Sample data uses a fixed November 20, 2026 snapshot. It is an example, not your recorded attendance.".into()));
    }

    pub fn apply_import(self) {
        let Some(timetable) = self.import_draft.get_untracked() else {
            return;
        };
        if let Err(error) = timetable.validate() {
            self.notice.set(Some(error.to_string()));
            return;
        }
        self.edit(|plan| {
            plan.timetable = timetable;
            // A replacement schedule invalidates the meaning of aggregate counts
            // and leave slots. Reset visibly only after the explicit Apply action.
            plan.attendance.attended.clear();
            plan.leave.clear();
        });
        self.import_draft.set(None);
        self.notice.set(Some("Timetable applied. Attendance counts and leave were reset; enter the counts for this schedule before planning.".into()));
    }

    pub fn cancel_import(self) {
        self.import_draft.set(None);
    }

    pub fn restore_device_draft(self, key: String) {
        #[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
        crate::drafts::restore(self, key);
        #[cfg(not(all(feature = "hydrate", target_arch = "wasm32")))]
        let _ = key;
    }

    pub fn delete_device_draft(self, key: String) {
        #[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
        crate::drafts::delete(self, key);
        #[cfg(not(all(feature = "hydrate", target_arch = "wasm32")))]
        let _ = key;
    }

    pub fn reload_cloud(self) {
        #[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
        crate::browser::reload_cloud(self);
    }
    pub fn retry_save(self) {
        #[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
        crate::browser::retry_save(self);
    }
    pub fn download_draft(self) {
        #[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
        crate::browser::download_draft(self);
    }
    pub fn logout(self) {
        #[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
        crate::browser::logout(self);
    }
    pub fn ask(self, question: String) {
        #[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
        crate::browser::ask(self, question);
        #[cfg(not(all(feature = "hydrate", target_arch = "wasm32")))]
        let _ = question;
    }
    pub fn local_login(self, email: String, password: String) {
        #[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
        crate::browser::local_login(self, email, password);
        #[cfg(not(all(feature = "hydrate", target_arch = "wasm32")))]
        let _ = (email, password);
    }
    #[cfg(feature = "hydrate")]
    pub fn import_file(self, file: web_sys::File) {
        #[cfg(target_arch = "wasm32")]
        crate::browser::import_file(self, file);
        #[cfg(not(target_arch = "wasm32"))]
        let _ = file;
    }
}

pub fn empty_plan(calendar: &Calendar) -> Plan {
    Plan {
        schema_version: 1,
        semester_id: calendar.config().id.clone(),
        calendar_revision: calendar.config().revision,
        timetable: Timetable::default(),
        attendance: AttendanceSnapshot {
            as_of: calendar.config().start_date.pred_opt().expect("valid term"),
            attended: Default::default(),
        },
        leave: Vec::new(),
    }
}

pub fn sample_plan() -> Plan {
    serde_json::from_str(include_str!("../data/sample-plan.json")).expect("tested sample plan")
}

fn counting_plan(mut plan: Plan) -> Plan {
    plan.attendance.attended.clear();
    plan.leave.clear();
    plan
}

pub fn set_cell(plan: &mut Plan, day: GridDay, index: usize, value: String) {
    if index >= 7 {
        return;
    }
    let code = value.trim().to_ascii_uppercase();
    let cell = if code.is_empty() {
        None
    } else {
        if !plan.timetable.courses.iter().any(|c| c.code == code) {
            plan.timetable.courses.push(Course {
                code: code.clone(),
                name: String::new(),
            });
        }
        Some(code)
    };
    plan.timetable
        .grid
        .entry(day)
        .or_insert_with(|| std::array::from_fn(|_| None))[index] = cell;
}

pub fn toggle_slot(plan: &mut Plan, date: NaiveDate, slot: SlotIndex, meetings: &[Meeting]) {
    let mut selected: std::collections::BTreeSet<_> = meetings
        .iter()
        .filter(|m| {
            plan.leave.iter().any(|entry| match entry {
                LeaveEntry::Day { date: d } => *d == date,
                LeaveEntry::Slot { date: d, slot } => *d == date && *slot == m.slot,
            })
        })
        .map(|m| m.slot)
        .collect();
    if !selected.remove(&slot) {
        selected.insert(slot);
    }
    plan.leave.retain(|entry| entry.date() != date);
    if !selected.is_empty() && selected.len() == meetings.len() {
        plan.leave.push(LeaveEntry::Day { date });
    } else {
        plan.leave.extend(
            selected
                .into_iter()
                .map(|slot| LeaveEntry::Slot { date, slot }),
        );
    }
}

fn remove_obsolete_leave(calendar: &Calendar, plan: &mut Plan) {
    let count_plan = counting_plan(plan.clone());
    if let Ok(engine) = Engine::new(calendar, &count_plan) {
        plan.leave.retain(|entry| {
            let meetings = engine.meetings_on(entry.date());
            match entry {
                LeaveEntry::Day { .. } => !meetings.is_empty(),
                LeaveEntry::Slot { slot, .. } => meetings.iter().any(|m| m.slot == *slot),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_one_slot_from_whole_day_preserves_other_courses() {
        let calendar = Calendar::winter_2026().unwrap();
        let mut plan = sample_plan();
        let date = NaiveDate::from_ymd_opt(2026, 11, 21).unwrap();
        plan.leave = vec![LeaveEntry::Day { date }];
        let meetings = Engine::new(&calendar, &plan)
            .unwrap()
            .meetings_on(date)
            .to_vec();
        toggle_slot(&mut plan, date, SlotIndex::new(0).unwrap(), &meetings);
        assert_eq!(
            plan.leave,
            vec![LeaveEntry::Slot {
                date,
                slot: SlotIndex::new(1).unwrap()
            }]
        );
        toggle_slot(&mut plan, date, SlotIndex::new(0).unwrap(), &meetings);
        assert_eq!(plan.leave, vec![LeaveEntry::Day { date }]);
    }

    #[test]
    fn edited_timetable_does_not_silently_change_attendance() {
        let mut plan = sample_plan();
        let recorded = plan.attendance.clone();
        set_cell(&mut plan, GridDay::Mon, 0, "new1001".into());
        assert_eq!(plan.attendance, recorded);
        assert_eq!(
            plan.timetable.grid[&GridDay::Mon][0].as_deref(),
            Some("NEW1001")
        );
        assert!(plan.timetable.courses.iter().any(|c| c.code == "NEW1001"));
    }
}

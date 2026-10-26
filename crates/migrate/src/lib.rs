//! Offline preparation has no environment or database access. Only `apply` and
//! `rollback` accept a database pool; callers must use an explicitly selected URL.
mod json;
pub mod storage;

use chrono::NaiveDate;
use debarred_domain::{load_semester, AttendanceSnapshot, Engine, LeaveEntry, Plan, Timetable};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub const REPORT_VERSION: u32 = 1;

/// Only fixed error codes are displayed: parser, domain, and SQL errors may
/// contain secrets or full input values and must never escape into CLI output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(pub &'static str);
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Export {
    source_kind: SourceKind,
    records: Vec<Record>,
}
#[derive(Debug, Deserialize)]
enum SourceKind {
    Clerk,
    Supabase,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    source_user_id: String,
    semester_id: String,
    timetable: Value,
    attended: Value,
    leave_dates: Value,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    mappings: Vec<Mapping>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mapping {
    source_user_id: String,
    target_user_id: Uuid,
    semester_id: String,
    snapshot_as_of: NaiveDate,
    verified_by: String,
    verification_reference: String,
}

#[derive(Debug, Serialize)]
pub struct Issue {
    pub source_user_id: Option<String>,
    pub code: &'static str,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub report_version: u32,
    pub source_sha256: String,
    pub mapping_sha256: String,
    pub record_count: usize,
    pub mapping_count: usize,
    pub prepared_count: usize,
    pub source_user_ids: Vec<String>,
    pub errors: Vec<Issue>,
    /// Exact confirmation token. Absent unless the entire batch is valid.
    pub prepared_digest: Option<String>,
}

/// Deliberately not deserializable or publicly constructible: mutations require
/// a fresh offline validation of source + manifest, not trust in a bundle file.
#[derive(Debug, Serialize)]
pub struct PreparedBatch {
    report_version: u32,
    source_sha256: String,
    mapping_sha256: String,
    prepared_digest: String,
    rows: Vec<PreparedRow>,
}
#[derive(Debug, Serialize)]
struct PreparedRow {
    source_user_id: String,
    target_user_id: Uuid,
    semester_id: String,
    plan: Plan,
    plan_sha256: String,
    plan_storage_sha256: String,
}
impl PreparedBatch {
    pub fn digest(&self) -> &str {
        &self.prepared_digest
    }
    pub fn len(&self) -> usize {
        self.rows.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}
pub struct Validation {
    pub report: Report,
    pub prepared: Option<PreparedBatch>,
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Canonical v1: recursively lexicographic JSON object keys, sorted courses,
/// and deduplicated sorted actual leave slots (whole days expand to meetings).
/// Plan storage preserves the original leave entries, including duplicates.
pub fn canonical_plan_sha256(plan: &Plan) -> Result<String> {
    let calendar = load_semester(&plan.semester_id).map_err(|_| Error("invalid_plan"))?;
    let engine = Engine::new(&calendar, plan).map_err(|_| Error("invalid_plan"))?;
    let mut canonical = plan.clone();
    canonical
        .timetable
        .courses
        .sort_by(|a, b| a.code.cmp(&b.code));
    let mut slots = BTreeSet::new();
    for entry in &plan.leave {
        match *entry {
            LeaveEntry::Day { date } => {
                for m in engine.meetings_on(date) {
                    slots.insert((date, m.slot));
                }
            }
            LeaveEntry::Slot { date, slot } => {
                slots.insert((date, slot));
            }
        }
    }
    canonical.leave = slots
        .into_iter()
        .map(|(date, slot)| LeaveEntry::Slot { date, slot })
        .collect();
    Ok(sha256(&canonical_bytes(&canonical)?))
}

/// Lossless JSON-value commitment for rollback: normalize object-key order only.
/// Arrays, duplicate array entries, and Day/Slot representations remain distinct.
/// Hash the stored JSON value directly, without a typed Plan round trip.
pub fn plan_storage_sha256(value: &Value) -> Result<String> {
    Ok(sha256(&canonical_bytes(value)?))
}

fn canonical_bytes(value: &impl Serialize) -> Result<Vec<u8>> {
    // Serialize the tree with explicit key ordering, independent of serde_json's
    // optional preserve_order feature being unified by another workspace crate.
    fn emit(value: &Value, out: &mut Vec<u8>) -> Result<()> {
        match value {
            Value::Object(map) => {
                out.push(b'{');
                let sorted: BTreeMap<_, _> = map.iter().collect();
                for (i, (k, v)) in sorted.into_iter().enumerate() {
                    if i != 0 {
                        out.push(b',');
                    }
                    out.extend(serde_json::to_vec(k).map_err(|_| Error("serialization_failed"))?);
                    out.push(b':');
                    emit(v, out)?;
                }
                out.push(b'}');
            }
            Value::Array(a) => {
                out.push(b'[');
                for (i, v) in a.iter().enumerate() {
                    if i != 0 {
                        out.push(b',');
                    }
                    emit(v, out)?;
                }
                out.push(b']');
            }
            v => out.extend(serde_json::to_vec(v).map_err(|_| Error("serialization_failed"))?),
        }
        Ok(())
    }
    let value = serde_json::to_value(value).map_err(|_| Error("serialization_failed"))?;
    let mut out = Vec::new();
    emit(&value, &mut out)?;
    Ok(out)
}

fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn verified(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control)
}

/// No implicit semester aliases, source UUID normalization, timestamps, email
/// matching, empty defaults, invalid-leave filtering, or partial batches.
pub fn validate(source: &[u8], mapping: &[u8]) -> Validation {
    let mut report = Report {
        report_version: REPORT_VERSION,
        source_sha256: sha256(source),
        mapping_sha256: sha256(mapping),
        record_count: 0,
        mapping_count: 0,
        prepared_count: 0,
        source_user_ids: vec![],
        errors: vec![],
        prepared_digest: None,
    };
    let prepared = prepare(source, mapping, &mut report);
    Validation { report, prepared }
}

fn issue(report: &mut Report, id: Option<&str>, code: &'static str) {
    report.errors.push(Issue {
        source_user_id: id.filter(|s| identifier(s)).map(str::to_owned),
        code,
    });
}
fn prepare(source: &[u8], mapping: &[u8], report: &mut Report) -> Option<PreparedBatch> {
    let export: Export =
        match json::parse(source).and_then(|v| serde_json::from_value(v).map_err(|_| ())) {
            Ok(v) => v,
            Err(_) => {
                issue(report, None, "invalid_source_json");
                return None;
            }
        };
    report.record_count = export.records.len();
    report.source_user_ids = export
        .records
        .iter()
        .map(|r| r.source_user_id.clone())
        .filter(|id| identifier(id))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let manifest: Manifest =
        match json::parse(mapping).and_then(|v| serde_json::from_value(v).map_err(|_| ())) {
            Ok(v) => v,
            Err(_) => {
                issue(report, None, "invalid_mapping_json_or_missing_confirmation");
                return None;
            }
        };
    report.mapping_count = manifest.mappings.len();
    if export.records.is_empty() {
        issue(report, None, "empty_batch");
    }
    let mut maps = BTreeMap::new();
    let mut destinations = BTreeSet::new();
    for m in manifest.mappings {
        if !identifier(&m.source_user_id)
            || m.target_user_id.is_nil()
            || !verified(&m.verified_by)
            || !verified(&m.verification_reference)
        {
            issue(
                report,
                Some(&m.source_user_id),
                "unverified_or_ambiguous_mapping",
            );
        }
        if !destinations.insert((m.target_user_id, m.semester_id.clone())) {
            issue(report, Some(&m.source_user_id), "duplicate_destination");
        }
        let id = m.source_user_id.clone();
        if maps.insert(id.clone(), m).is_some() {
            issue(report, Some(&id), "duplicate_source_mapping");
        }
    }
    let mut seen = BTreeSet::new();
    let mut rows = Vec::new();
    for record in export.records {
        let id = record.source_user_id.as_str();
        let valid_id = identifier(id)
            && match export.source_kind {
                SourceKind::Clerk => id.starts_with("user_") && id.len() > 5,
                SourceKind::Supabase => {
                    Uuid::parse_str(id).is_ok_and(|uuid| !uuid.is_nil() && uuid.to_string() == id)
                }
            };
        if !valid_id {
            issue(report, Some(id), "ambiguous_source_id");
        }
        if !seen.insert(id.to_owned()) {
            issue(report, Some(id), "duplicate_source_record");
        }
        let Some(m) = maps.get(id) else {
            issue(report, Some(id), "unmapped_source_record");
            continue;
        };
        if record.semester_id != m.semester_id || record.semester_id != "winter-2026" {
            issue(report, Some(id), "semester_mismatch_or_unsupported");
            continue;
        }
        let plan = (|| -> Result<Plan> {
            let calendar =
                load_semester(&m.semester_id).map_err(|_| Error("unsupported_semester"))?;
            let timetable: Timetable =
                json::field(record.timetable).map_err(|_| Error("invalid_timetable"))?;
            let attended: BTreeMap<String, u32> =
                json::field(record.attended).map_err(|_| Error("invalid_attendance"))?;
            let entries: Vec<String> =
                json::field(record.leave_dates).map_err(|_| Error("invalid_leave_dates"))?;
            let leave = entries
                .iter()
                .map(|s| LeaveEntry::from_legacy(s).map_err(|_| Error("invalid_leave_entry")))
                .collect::<Result<Vec<_>>>()?;
            let plan = Plan {
                schema_version: 1,
                semester_id: m.semester_id.clone(),
                calendar_revision: calendar.config().revision,
                timetable,
                attendance: AttendanceSnapshot {
                    as_of: m.snapshot_as_of,
                    attended,
                },
                leave,
            };
            Engine::new(&calendar, &plan).map_err(|_| Error("invalid_plan_engine"))?;
            Ok(plan)
        })();
        match plan.and_then(|plan| {
            let value = serde_json::to_value(&plan).map_err(|_| Error("serialization_failed"))?;
            Ok((
                canonical_plan_sha256(&plan)?,
                plan_storage_sha256(&value)?,
                plan,
            ))
        }) {
            Ok((plan_sha256, plan_storage_sha256, plan)) => rows.push(PreparedRow {
                source_user_id: id.to_owned(),
                target_user_id: m.target_user_id,
                semester_id: m.semester_id.clone(),
                plan,
                plan_sha256,
                plan_storage_sha256,
            }),
            Err(e) => issue(report, Some(id), e.0),
        }
    }
    for id in maps.keys() {
        if !seen.contains(id) {
            issue(report, Some(id), "mapping_without_source_record");
        }
    }
    report.source_user_ids = seen.into_iter().filter(|s| identifier(s)).collect();
    report.prepared_count = rows.len();
    if !report.errors.is_empty() {
        return None;
    }
    rows.sort_by(|a, b| {
        (a.target_user_id, &a.semester_id).cmp(&(b.target_user_id, &b.semester_id))
    });
    let commitments: Vec<_> = rows
        .iter()
        .map(|r| {
            (
                &r.source_user_id,
                r.target_user_id,
                &r.semester_id,
                &r.plan_sha256,
                &r.plan_storage_sha256,
            )
        })
        .collect();
    let digest = sha256(
        &canonical_bytes(&(
            REPORT_VERSION,
            &report.source_sha256,
            &report.mapping_sha256,
            commitments,
        ))
        .ok()?,
    );
    report.prepared_digest = Some(digest.clone());
    Some(PreparedBatch {
        report_version: REPORT_VERSION,
        source_sha256: report.source_sha256.clone(),
        mapping_sha256: report.mapping_sha256.clone(),
        prepared_digest: digest,
        rows,
    })
}

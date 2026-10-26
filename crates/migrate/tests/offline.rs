use debarred_domain::{LeaveEntry, Plan};
use debarred_migrate::{
    canonical_plan_sha256, plan_storage_sha256, storage::connection_options, validate,
};
use serde_json::{json, Value};
use std::{fs, process::Command};
use uuid::Uuid;

fn fixture() -> (Value, Value) {
    (
        json!({"source_kind":"Clerk", "records":[{
            "source_user_id":"user_synthetic", "semester_id":"winter-2026",
            "timetable":{"courses":[{"code":"CS101","name":"Synthetic course"}],
                "grid":{"Mon":["CS101",null,null,null,null,null,null]}},
            "attended":{"CS101":1}, "leave_dates":["2026-11-23", "2026-11-30#0"]
        }]}),
        json!({"mappings":[{
            "source_user_id":"user_synthetic", "target_user_id":"00000000-0000-4000-8000-000000000001",
            "semester_id":"winter-2026", "snapshot_as_of":"2026-11-16",
            "verified_by":"Synthetic operator", "verification_reference":"Synthetic manual proof 1"
        }]}),
    )
}
fn check(source: &Value, map: &Value) -> debarred_migrate::Validation {
    validate(
        &serde_json::to_vec(source).unwrap(),
        &serde_json::to_vec(map).unwrap(),
    )
}
fn rejected(source: &Value, map: &Value) {
    let v = check(source, map);
    assert!(v.prepared.is_none());
    assert!(v.report.prepared_digest.is_none());
    assert!(!v.report.errors.is_empty());
}
#[test]
fn leave_adapter_is_lossless_and_all_legacy_fields_accept_one_json_encoding() {
    let (mut source, map) = fixture();
    let result = check(&source, &map);
    let batch = result.prepared.unwrap();
    let bundle = serde_json::to_value(&batch).unwrap();
    let plan: Plan = serde_json::from_value(bundle["rows"][0]["plan"].clone()).unwrap();
    assert_eq!(
        plan.leave.iter().map(|e| e.to_legacy()).collect::<Vec<_>>(),
        ["2026-11-23", "2026-11-30#0"]
    );
    for field in ["timetable", "attended", "leave_dates"] {
        source["records"][0][field] = Value::String(source["records"][0][field].to_string());
    }
    let encoded = check(&source, &map).prepared.unwrap();
    assert_ne!(batch.digest(), encoded.digest()); // Raw source bytes are committed.
    assert_eq!(
        bundle["rows"][0]["plan_sha256"],
        serde_json::to_value(encoded).unwrap()["rows"][0]["plan_sha256"]
    );
    let mut equivalent = plan.clone();
    equivalent.leave = vec![
        LeaveEntry::from_legacy("2026-11-30#0").unwrap(),
        LeaveEntry::from_legacy("2026-11-23#0").unwrap(),
        LeaveEntry::from_legacy("2026-11-23#0").unwrap(),
    ];
    assert_eq!(
        canonical_plan_sha256(&plan).unwrap(),
        canonical_plan_sha256(&equivalent).unwrap()
    );
}
#[test]
fn requires_snapshot_verified_mapping_and_rejects_unknown_fields() {
    let (source, map) = fixture();
    for field in [
        "snapshot_as_of",
        "verified_by",
        "verification_reference",
        "target_user_id",
    ] {
        let mut bad = map.clone();
        bad["mappings"][0].as_object_mut().unwrap().remove(field);
        rejected(&source, &bad);
    }
    let mut bad = map.clone();
    bad["mappings"][0]["verified_by"] = json!("  ");
    rejected(&source, &bad);
    let mut bad = source.clone();
    bad["records"][0]["email"] = json!("synthetic@example.invalid");
    rejected(&bad, &map);
    let mut bad = source.clone();
    bad["records"][0]["created_at"] = json!("2026-11-16");
    rejected(&bad, &map);
    let mut bad = source.clone();
    bad["records"][0]["semester_id"] = json!("fall-2026");
    rejected(&bad, &map);
}
#[test]
fn counts_dates_identifiers_and_invalid_leave_are_never_repaired() {
    let (source, map) = fixture();
    for counts in [
        json!({"CS101":-1}),
        json!({"CS101":1.5}),
        json!({"CS101":1000}),
        json!({"UNKNOWN":0}),
    ] {
        let mut bad = source.clone();
        bad["records"][0]["attended"] = counts;
        rejected(&bad, &map);
    }
    for leave in [
        "2026-11-23#7",
        "2026-11-23#1",
        "2026-11-22",
        "2025-11-23",
        "2026-13-23",
        "2026-11-23#00",
    ] {
        let mut bad = source.clone();
        bad["records"][0]["leave_dates"] = json!([leave]);
        rejected(&bad, &map);
    }
    let mut bad = source.clone();
    bad["records"][0]["source_user_id"] = json!(" user_synthetic");
    rejected(&bad, &map);
    let mut bad = source.clone();
    bad["source_kind"] = json!("Supabase");
    rejected(&bad, &map);
    let mut bad = source.clone();
    bad["records"][0]["timetable"]["grid"]["Mon"] = json!(["CS101"]);
    rejected(&bad, &map);
}
#[test]
fn duplicates_extra_and_unmapped_records_abort_the_whole_batch() {
    let (source, map) = fixture();
    let mut bad = source.clone();
    bad["records"]
        .as_array_mut()
        .unwrap()
        .push(source["records"][0].clone());
    rejected(&bad, &map);
    let mut bad = map.clone();
    bad["mappings"]
        .as_array_mut()
        .unwrap()
        .push(map["mappings"][0].clone());
    rejected(&source, &bad);
    let mut bad = map.clone();
    let mut second = map["mappings"][0].clone();
    second["source_user_id"] = json!("user_other");
    bad["mappings"].as_array_mut().unwrap().push(second);
    rejected(&source, &bad);
    rejected(&source, &json!({"mappings":[]}));
    rejected(&json!({"source_kind":"Clerk","records":[]}), &map);
    let duplicate = serde_json::to_string(&source).unwrap().replace(
        "\"attended\":{\"CS101\":1}",
        "\"attended\":{\"CS101\":1,\"CS101\":0}",
    );
    assert!(
        validate(duplicate.as_bytes(), &serde_json::to_vec(&map).unwrap())
            .prepared
            .is_none()
    );
    let mut bad = source.clone();
    bad["records"][0]["attended"] = json!("{\"CS101\":1,\"CS101\":0}");
    rejected(&bad, &map);
}
#[test]
fn report_contains_commitments_not_plan_or_verification_data() {
    let (source, map) = fixture();
    let first = check(&source, &map);
    let second = check(&source, &map);
    assert_eq!(first.report.prepared_digest, second.report.prepared_digest);
    let report = serde_json::to_string(&first.report).unwrap();
    for secret in [
        "Synthetic course",
        "CS101",
        "manual proof",
        "snapshot_as_of",
        "target_user_id",
    ] {
        assert!(!report.contains(secret));
    }
    let mut changed = map.clone();
    changed["mappings"][0]["verification_reference"] = json!("New confirmation");
    assert_ne!(
        first.report.prepared_digest,
        check(&source, &changed).report.prepared_digest
    );
}
#[test]
fn remote_is_opt_in_and_remote_tls_is_always_verify_full() {
    assert!(connection_options(
        "postgres://u:secret@remote.invalid/db?sslmode=disable",
        false
    )
    .is_err());
    let remote = connection_options(
        "postgres://u:secret@remote.invalid/db?sslmode=disable",
        true,
    )
    .unwrap();
    assert!(matches!(
        remote.get_ssl_mode(),
        sqlx::postgres::PgSslMode::VerifyFull
    ));
    assert!(connection_options("postgres://u@127.0.0.1/db", false).is_ok());
    assert!(connection_options("postgres://u@localhost/db?host=remote.invalid", false).is_err());
}
#[test]
fn cli_default_dry_run_ignores_remote_database_env_and_needs_no_app_config() {
    let dir = std::env::temp_dir().join(format!("debarred-migrate-test-{}", Uuid::new_v4()));
    fs::create_dir(&dir).unwrap();
    let (source, map) = fixture();
    fs::write(dir.join("source.json"), source.to_string()).unwrap();
    fs::write(dir.join("map.json"), map.to_string()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_debarred-migrate"))
        .env_clear()
        .env(
            "MIGRATION_DATABASE_URL",
            "postgres://operator:DO_NOT_PRINT@remote.invalid/db",
        )
        .env(
            "DATABASE_URL",
            "postgres://operator:DO_NOT_PRINT@remote.invalid/db",
        )
        .args([
            "--input",
            "source.json",
            "--mapping",
            "map.json",
            "--report",
            "report.json",
        ])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("DO_NOT_PRINT"));
    assert!(dir.join("report.json").exists());
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 3); // No bundle without an explicit path.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(dir.join("report.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let wrong_digest = Command::new(env!("CARGO_BIN_EXE_debarred-migrate"))
        .env_clear()
        .env(
            "MIGRATION_DATABASE_URL",
            "postgres://operator:DO_NOT_PRINT@remote.invalid/db",
        )
        .args([
            "apply",
            "--input",
            "source.json",
            "--mapping",
            "map.json",
            "--report",
            "wrong-digest.json",
            "--confirm-digest",
            "wrong",
        ])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(!wrong_digest.status.success());
    assert_eq!(
        String::from_utf8_lossy(&wrong_digest.stderr).trim(),
        "migration error: confirmation_digest_mismatch"
    );
    let digest = check(&source, &map).prepared.unwrap().digest().to_owned();
    let no_fallback = Command::new(env!("CARGO_BIN_EXE_debarred-migrate"))
        .env_clear()
        .env(
            "DATABASE_URL",
            "postgres://operator:DO_NOT_PRINT@remote.invalid/db",
        )
        .args([
            "apply",
            "--input",
            "source.json",
            "--mapping",
            "map.json",
            "--report",
            "no-fallback.json",
            "--confirm-digest",
            &digest,
        ])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(!no_fallback.status.success());
    assert_eq!(
        String::from_utf8_lossy(&no_fallback.stderr).trim(),
        "migration error: migration_database_url_required"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn storage_commitment_normalizes_only_object_key_order() {
    let a: Value = serde_json::from_str(r#"{"a":[{"x":1,"y":2},null],"b":{"m":3,"n":4}}"#).unwrap();
    let reordered: Value =
        serde_json::from_str(r#"{"b":{"n":4,"m":3},"a":[{"y":2,"x":1},null]}"#).unwrap();
    assert_eq!(
        plan_storage_sha256(&a).unwrap(),
        plan_storage_sha256(&reordered).unwrap()
    );
    let mut changed = a.clone();
    changed["a"].as_array_mut().unwrap().reverse();
    assert_ne!(
        plan_storage_sha256(&a).unwrap(),
        plan_storage_sha256(&changed).unwrap()
    );

    let (mut source, map) = fixture();
    source["records"][0]["timetable"]["courses"]
        .as_array_mut()
        .unwrap()
        .push(json!({"code":"CS102","name":"Synthetic two"}));
    let prepared = serde_json::to_value(check(&source, &map).prepared.unwrap()).unwrap();
    let original = prepared["rows"][0]["plan"].clone();
    assert_eq!(
        prepared["rows"][0]["plan_storage_sha256"],
        plan_storage_sha256(&original).unwrap()
    );
    for change in ["course_order", "day_to_slot", "duplicate_leave"] {
        let mut edited = original.clone();
        match change {
            "course_order" => edited["timetable"]["courses"]
                .as_array_mut()
                .unwrap()
                .reverse(),
            "day_to_slot" => {
                edited["leave"][0] = json!({"kind":"slot","date":"2026-11-23","slot":0})
            }
            "duplicate_leave" => {
                let entry = edited["leave"][0].clone();
                edited["leave"].as_array_mut().unwrap().push(entry);
            }
            _ => unreachable!(),
        }
        assert_eq!(
            canonical_plan_sha256(&serde_json::from_value(original.clone()).unwrap()).unwrap(),
            canonical_plan_sha256(&serde_json::from_value(edited.clone()).unwrap()).unwrap()
        );
        assert_ne!(
            plan_storage_sha256(&original).unwrap(),
            plan_storage_sha256(&edited).unwrap(),
            "{change}"
        );
    }
}

#[test]
fn cli_reports_and_bundles_are_private_and_never_overwrite_existing_files() {
    let dir =
        std::env::temp_dir().join(format!("debarred-migrate-private-test-{}", Uuid::new_v4()));
    fs::create_dir(&dir).unwrap();
    let (source, map) = fixture();
    fs::write(dir.join("source.json"), source.to_string()).unwrap();
    fs::write(dir.join("map.json"), map.to_string()).unwrap();
    let run = |report: &str, bundle: &str| {
        Command::new(env!("CARGO_BIN_EXE_debarred-migrate"))
            .env_clear()
            .args([
                "validate",
                "--input",
                "source.json",
                "--mapping",
                "map.json",
                "--report",
                report,
                "--prepared-bundle",
                bundle,
            ])
            .current_dir(&dir)
            .output()
            .unwrap()
    };
    let output = run("report.json", "bundle.json");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = fs::read(dir.join("report.json")).unwrap();
    let bundle = fs::read(dir.join("bundle.json")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for file in ["report.json", "bundle.json"] {
            assert_eq!(
                fs::metadata(dir.join(file)).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    assert!(!run("report.json", "bundle.json").status.success());
    assert_eq!(fs::read(dir.join("report.json")).unwrap(), report);
    assert_eq!(fs::read(dir.join("bundle.json")).unwrap(), bundle);
    fs::write(
        dir.join("operator-existing.json"),
        b"operator original bytes",
    )
    .unwrap();
    assert!(!run("new-report.json", "operator-existing.json")
        .status
        .success());
    assert_eq!(
        fs::read(dir.join("operator-existing.json")).unwrap(),
        b"operator original bytes"
    );
    assert_eq!(
        fs::read(dir.join("source.json")).unwrap(),
        source.to_string().as_bytes()
    );
    assert_eq!(
        fs::read(dir.join("map.json")).unwrap(),
        map.to_string().as_bytes()
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn supabase_source_ids_are_canonical_and_still_require_explicit_mapping() {
    let (mut source, mut map) = fixture();
    let id = "00000000-0000-4000-8000-00000000000a";
    source["source_kind"] = json!("Supabase");
    source["records"][0]["source_user_id"] = json!(id);
    map["mappings"][0]["source_user_id"] = json!(id);
    assert!(check(&source, &map).prepared.is_some());
    rejected(&source, &json!({"mappings":[]}));
    for alias in [id.to_uppercase(), id.replace('-', "")] {
        source["records"][0]["source_user_id"] = json!(alias);
        map["mappings"][0]["source_user_id"] = source["records"][0]["source_user_id"].clone();
        rejected(&source, &map);
    }
}

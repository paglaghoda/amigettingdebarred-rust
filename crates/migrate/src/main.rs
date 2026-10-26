use debarred_migrate::{storage, validate, Error, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::Write,
    path::Path,
};
use uuid::Uuid;

const HELP: &str = "debarred-migrate [validate] --input EXPORT.json --mapping MAPPING.json --report REPORT.json [--prepared-bundle BUNDLE.json]
debarred-migrate apply --input EXPORT.json --mapping MAPPING.json --report REPORT.json --confirm-digest SHA256 [--prepared-bundle BUNDLE.json] [--allow-remote]
debarred-migrate rollback --batch-id UUID --confirm-digest SHA256 [--allow-remote]

validate is the default and NEVER reads database configuration or connects.
apply/rollback read only MIGRATION_DATABASE_URL. Remote access requires --allow-remote and uses VerifyFull TLS.
Output files must not already exist; reports and bundles are operator-private.";

struct Args {
    mode: String,
    values: BTreeMap<String, String>,
    allow_remote: bool,
}
fn args() -> Result<Option<Args>> {
    let mut args = std::env::args().skip(1).peekable();
    if args.peek().is_some_and(|s| s == "--help" || s == "-h") {
        println!("{HELP}");
        return Ok(None);
    }
    let mode = if args.peek().is_some_and(|s| !s.starts_with('-')) {
        args.next().unwrap()
    } else {
        "validate".into()
    };
    if !matches!(mode.as_str(), "validate" | "apply" | "rollback") {
        return Err(Error("invalid_mode_use_help"));
    }
    let mut values = BTreeMap::new();
    let mut allow_remote = false;
    while let Some(key) = args.next() {
        if key == "--allow-remote" {
            if allow_remote || mode == "validate" {
                return Err(Error("invalid_or_duplicate_option"));
            }
            allow_remote = true;
            continue;
        }
        let allowed = match mode.as_str() {
            "rollback" => ["--batch-id", "--confirm-digest"].as_slice(),
            "apply" => [
                "--input",
                "--mapping",
                "--report",
                "--prepared-bundle",
                "--confirm-digest",
            ]
            .as_slice(),
            _ => ["--input", "--mapping", "--report", "--prepared-bundle"].as_slice(),
        };
        if !allowed.contains(&key.as_str()) {
            return Err(Error("unknown_option_use_help"));
        }
        let value = args
            .next()
            .filter(|s| !s.starts_with("--"))
            .ok_or(Error("missing_option_value"))?;
        if values.insert(key, value).is_some() {
            return Err(Error("duplicate_option"));
        }
    }
    Ok(Some(Args {
        mode,
        values,
        allow_remote,
    }))
}
fn required<'a>(args: &'a Args, key: &str) -> Result<&'a str> {
    args.values
        .get(key)
        .map(String::as_str)
        .ok_or(Error("required_option_missing_use_help"))
}
fn read(path: &str) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|_| Error("input_file_read_failed"))
}
fn write_private(path: &str, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|_| Error("output_file_create_failed_must_be_new"))?;
    file.write_all(bytes)
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.sync_all())
        .map_err(|_| Error("output_file_write_failed"))
}
fn serialized(value: &impl serde::Serialize) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(value).map_err(|_| Error("serialization_failed"))
}
fn database_url() -> Result<String> {
    std::env::var("MIGRATION_DATABASE_URL").map_err(|_| Error("migration_database_url_required"))
}

async fn run() -> Result<()> {
    let Some(args) = args()? else {
        return Ok(());
    };
    if args.mode == "rollback" {
        let batch_id = Uuid::parse_str(required(&args, "--batch-id")?)
            .map_err(|_| Error("invalid_batch_id"))?;
        let digest = required(&args, "--confirm-digest")?;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(Error("invalid_confirmation_digest"));
        }
        let pool = storage::connect(&database_url()?, args.allow_remote).await?;
        storage::rollback(&pool, batch_id, digest).await?;
        println!("rolled_back batch_id={batch_id}");
        return Ok(());
    }
    let input = required(&args, "--input")?;
    let mapping = required(&args, "--mapping")?;
    let report_path = required(&args, "--report")?;
    let mut paths = BTreeSet::new();
    for path in [
        Some(input),
        Some(mapping),
        Some(report_path),
        args.values.get("--prepared-bundle").map(String::as_str),
    ]
    .into_iter()
    .flatten()
    {
        if !paths.insert(Path::new(path)) {
            return Err(Error("input_and_output_paths_must_differ"));
        }
    }
    let validation = validate(&read(input)?, &read(mapping)?);
    write_private(report_path, &serialized(&validation.report)?)?;
    let batch = validation
        .prepared
        .ok_or(Error("validation_failed_see_private_report"))?;
    if let Some(path) = args.values.get("--prepared-bundle") {
        write_private(path, &serialized(&batch)?)?;
    }
    if args.mode == "validate" {
        println!(
            "validated records={} prepared_digest={}",
            batch.len(),
            batch.digest()
        );
        return Ok(());
    }
    let confirm = required(&args, "--confirm-digest")?;
    // Validate confirmation before even reading database configuration.
    if confirm != batch.digest() {
        return Err(Error("confirmation_digest_mismatch"));
    }
    let pool = storage::connect(&database_url()?, args.allow_remote).await?;
    let id = storage::apply(&pool, &batch, confirm).await?;
    println!("applied batch_id={id} prepared_digest={}", batch.digest());
    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("migration error: {error}");
        std::process::exit(1);
    }
}

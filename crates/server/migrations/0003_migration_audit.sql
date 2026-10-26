-- No legacy identifiers, verification documents, emails, or plan copies belong here.
CREATE TABLE app_private.migration_batches (
    batch_id uuid PRIMARY KEY,
    report_version integer NOT NULL CHECK (report_version = 1),
    fingerprint text NOT NULL CHECK (fingerprint ~ '^[0-9a-f]{64}$'),
    source_sha256 text NOT NULL CHECK (source_sha256 ~ '^[0-9a-f]{64}$'),
    mapping_sha256 text NOT NULL CHECK (mapping_sha256 ~ '^[0-9a-f]{64}$'),
    row_count bigint NOT NULL CHECK (row_count > 0),
    status text NOT NULL CHECK (status IN ('applied', 'rolled_back')),
    applied_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    rolled_back_at timestamptz,
    CHECK ((status = 'rolled_back') = (rolled_back_at IS NOT NULL))
);
CREATE TABLE app_private.migration_rows (
    batch_id uuid NOT NULL REFERENCES app_private.migration_batches(batch_id),
    user_id uuid NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
    semester_id text NOT NULL,
    inserted_revision bigint NOT NULL CHECK (inserted_revision = 1),
    plan_sha256 text NOT NULL CHECK (plan_sha256 ~ '^[0-9a-f]{64}$'),
    plan_storage_sha256 text NOT NULL CHECK (plan_storage_sha256 ~ '^[0-9a-f]{64}$'),
    PRIMARY KEY (batch_id, user_id, semester_id)
);
CREATE INDEX migration_rows_user_id_idx ON app_private.migration_rows(user_id);
ALTER TABLE app_private.migration_batches ENABLE ROW LEVEL SECURITY;
ALTER TABLE app_private.migration_rows ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON app_private.migration_batches, app_private.migration_rows FROM PUBLIC;
DO $$
DECLARE api_role text;
BEGIN
    FOREACH api_role IN ARRAY ARRAY['anon', 'authenticated'] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role) THEN
            EXECUTE format('REVOKE ALL ON app_private.migration_batches, app_private.migration_rows FROM %I', api_role);
        END IF;
    END LOOP;
END
$$;

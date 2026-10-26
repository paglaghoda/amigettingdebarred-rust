ALTER TABLE app_private.auth_sessions
    ADD COLUMN authenticated_at timestamptz NOT NULL DEFAULT '-infinity';
ALTER TABLE app_private.auth_sessions ALTER COLUMN authenticated_at SET DEFAULT now();

CREATE TABLE app_private.account_deletion_requests (
    user_id uuid PRIMARY KEY REFERENCES auth.users(id) ON DELETE CASCADE,
    requested_at timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE app_private.account_deletion_requests ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON app_private.account_deletion_requests FROM PUBLIC;

-- Server write transactions call this before any row DML. Separate statements at
-- READ COMMITTED ensure the pending check sees a deletion committed while waiting.
CREATE FUNCTION app_private.lock_account_write(account_id uuid) RETURNS void
-- Run the queue check as its migration-role owner, not under caller RLS:
-- an invisible pending marker must never be mistaken for an absent marker.
LANGUAGE plpgsql VOLATILE SECURITY DEFINER
SET search_path = pg_catalog, app_private
AS $$
BEGIN
    PERFORM pg_catalog.pg_advisory_xact_lock(pg_catalog.hashtextextended(account_id::pg_catalog.text, 0));
    IF EXISTS (SELECT 1 FROM app_private.account_deletion_requests WHERE user_id = account_id) THEN
        RAISE EXCEPTION 'account identity deletion is pending' USING ERRCODE = '42501';
    END IF;
END
$$;

CREATE FUNCTION app_private.guard_account_write() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    -- Ownership is immutable; changing it would violate the server lock order.
    IF TG_OP = 'UPDATE' AND NEW.user_id IS DISTINCT FROM OLD.user_id THEN
        RAISE EXCEPTION 'account ownership cannot change' USING ERRCODE = '42501';
    END IF;
    PERFORM app_private.lock_account_write(NEW.user_id);
    RETURN NEW;
END
$$;
CREATE TRIGGER student_plans_account_guard BEFORE INSERT OR UPDATE
    ON app_private.student_plans FOR EACH ROW EXECUTE FUNCTION app_private.guard_account_write();
CREATE TRIGGER ai_usage_account_guard BEFORE INSERT OR UPDATE
    ON app_private.ai_usage FOR EACH ROW EXECUTE FUNCTION app_private.guard_account_write();
-- Refresh only updates an existing row: never introduce a row -> advisory lock
-- edge here. Deletion waits for refresh, then removes that row.
CREATE TRIGGER auth_sessions_account_guard BEFORE INSERT
    ON app_private.auth_sessions FOR EACH ROW EXECUTE FUNCTION app_private.guard_account_write();

REVOKE ALL ON FUNCTION app_private.lock_account_write(uuid) FROM PUBLIC;
REVOKE ALL ON FUNCTION app_private.guard_account_write() FROM PUBLIC;
DO $$
DECLARE api_role text;
BEGIN
    FOREACH api_role IN ARRAY ARRAY['anon', 'authenticated'] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role) THEN
            EXECUTE format('REVOKE ALL ON app_private.account_deletion_requests FROM %I', api_role);
            EXECUTE format('REVOKE ALL ON FUNCTION app_private.lock_account_write(uuid) FROM %I', api_role);
            EXECUTE format('REVOKE ALL ON FUNCTION app_private.guard_account_write() FROM %I', api_role);
        END IF;
    END LOOP;
END
$$;

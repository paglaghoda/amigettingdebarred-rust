CREATE SCHEMA app_private;
REVOKE ALL ON SCHEMA app_private FROM PUBLIC;

CREATE TABLE app_private.student_plans (
    user_id uuid NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
    semester_id text NOT NULL,
    plan jsonb NOT NULL CHECK (jsonb_typeof(plan) = 'object'),
    revision bigint NOT NULL CHECK (revision > 0),
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (user_id, semester_id)
);

CREATE TABLE app_private.auth_sessions (
    token_hash bytea PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
    email text,
    display_name text,
    encrypted_tokens bytea NOT NULL,
    access_expires_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL
);
CREATE INDEX auth_sessions_expires_at_idx ON app_private.auth_sessions (expires_at);
CREATE INDEX auth_sessions_user_id_idx ON app_private.auth_sessions (user_id);

CREATE TABLE app_private.oauth_flows (
    token_hash bytea PRIMARY KEY,
    encrypted_verifier bytea NOT NULL,
    expires_at timestamptz NOT NULL
);
CREATE INDEX oauth_flows_expires_at_idx ON app_private.oauth_flows (expires_at);

CREATE TABLE app_private.ai_usage (
    user_id uuid NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
    usage_day date NOT NULL,
    kind text NOT NULL CHECK (kind IN ('helper', 'image')),
    used integer NOT NULL CHECK (used >= 0),
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (user_id, usage_day, kind)
);

ALTER TABLE app_private.student_plans ENABLE ROW LEVEL SECURITY;
ALTER TABLE app_private.auth_sessions ENABLE ROW LEVEL SECURITY;
ALTER TABLE app_private.oauth_flows ENABLE ROW LEVEL SECURITY;
ALTER TABLE app_private.ai_usage ENABLE ROW LEVEL SECURITY;

REVOKE ALL ON ALL TABLES IN SCHEMA app_private FROM PUBLIC;
ALTER DEFAULT PRIVILEGES IN SCHEMA app_private REVOKE ALL ON TABLES FROM PUBLIC;

DO $$
DECLARE
    api_role text;
BEGIN
    FOREACH api_role IN ARRAY ARRAY['anon', 'authenticated'] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role) THEN
            EXECUTE format('REVOKE ALL ON SCHEMA app_private FROM %I', api_role);
            EXECUTE format('REVOKE ALL ON ALL TABLES IN SCHEMA app_private FROM %I', api_role);
            EXECUTE format('ALTER DEFAULT PRIVILEGES IN SCHEMA app_private REVOKE ALL ON TABLES FROM %I', api_role);
        END IF;
    END LOOP;
END
$$;

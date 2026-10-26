# Server integration

Run from the workspace root:

```sh
cargo run -p debarred-server -- --preview
cargo run -p debarred-server -- --migrate
cargo run -p debarred-server
```

`--preview` and `--migrate` are mutually exclusive; unknown or repeated arguments are errors.

## Credential-free preview

Preview binds to `127.0.0.1:3000` (`PREVIEW_PORT` optionally changes the port, not the interface). It does not load `Config`, read dotenv files, connect to PostgreSQL, start maintenance, or construct authentication/AI clients. It serves SSR pages, public assets, the real semester calendar, and `/api/runtime`. `/api/me` returns 401; authentication and private API operations return 503. It is not a production authentication bypass. Demo computation and device storage belong to the browser's shared Rust engine; preview does not save plans.

## Normal server

Normal startup retains the existing environment configuration and database migration requirements. `/api/runtime` exposes only public capabilities and the current campus date; session identity and CSRF state are fetched separately through `/api/me`, never embedded in SSR.

`LOCAL_AUTH_ENABLED` defaults to `false`. Enabling it requires **both** `PUBLIC_ORIGIN` and `SUPABASE_URL` to use HTTP loopback hosts (`localhost`, loopback IPv4, or loopback IPv6); other configurations fail validation at startup. `POST /auth/local/login` accepts `{ "email": "...", "password": "..." }`, requires exactly one matching `Origin`, and delegates password validation to Supabase. Successful login returns 204 with the same encrypted server-session cookie used by Google login. It shares the anonymous authentication rate budget and has an 8 KiB body limit. No fake users or client-supplied user IDs are accepted. Google callbacks still redirect to `/setup`.

Authenticated clients may send `X-Session-User` with the UUID currently displayed by the page. If present, exactly one valid UUID must match the cookie-authenticated user; otherwise reads, writes, refresh, and logout return 401 before private operations. This is only a stale-page precondition: ownership always comes from the encrypted server session, never the header. Omitting the header remains supported; `/api/me` omits it so the browser can discover a changed session.

## Frontend assets

Build with `python3 scripts/build-web.py` using the lockfile-matching wasm-bindgen CLI. Set `FRONTEND_DIST` to the generated dist directory; the default is `crates/web/dist`. Only its `assets` subtree is mounted at `/assets`, not the workspace. Docker builds these assets and sets `FRONTEND_DIST=/app/frontend` in the non-root runtime image.

Only `/`, `/setup`, `/planner`, `/demo`, `/demo/setup`, and `/demo/planner` render SSR documents. Unknown routes and missing assets never fall back to HTML. Pages render even before assets are built; missing CSS/bootstrap files return 404, so a frontend build is necessary for styling and hydration. HTML permits only same-origin external scripts and styles, plus `'wasm-unsafe-eval'` for WebAssembly compilation. JavaScript `'unsafe-eval'` and `'unsafe-inline'` remain prohibited. API responses retain their restrictive CSP and no-store policy.

## Validation

```sh
cargo test --locked -p debarred-server
cargo test --locked -p debarred-server -- --ignored
```

The ignored tests require `TEST_DATABASE_URL` pointing to a disposable real PostgreSQL database, and use an in-process mock provider for authentication exchanges. They do not authenticate against a hosted Supabase instance.

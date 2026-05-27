# Atomic Cloud — Postgres-backed Multi-Tenant Hosting

## Status

Living plan, iterating section by section. Sections marked **[drafted]** have
been worked through; sections marked **[stub]** are placeholders for future
deep dives. Decisions made so far are recorded in the "Decisions log" at the
bottom — when you change one, update the log so future-us knows what shifted.

## Context

The earlier exploration of "Atomic Cloud on SQLite + per-customer mounted
volumes" was working around the wrong constraint. Cloud users are by
definition trading privacy for ease of access; given that, the natural
storage choice is the Postgres engine we already built (see
`crates/atomic-core/src/storage/postgres/`). It ships with pgvector,
native-async sqlx, advisory-locked migrations, and a `db_id` column that
scopes per-tenant data — enough that the storage layer is essentially
feature-complete for cloud already. The work is the layer *above* it:
accounts, auth, tenant routing, provisioning, worker fairness, billing.

Self-hosted Atomic stays on SQLite. Cloud is purely Postgres. The storage
trait abstraction earns its keep here — same `atomic-core`, two deployment
shapes.

## Goals

- Multi-tenant hosting on a shared Postgres cluster, one database per
  account.
- Account-scoped auth, OAuth, sessions, and MCP integration.
- Cloud code lives in its own crate; `atomic-core` is untouched and
  `atomic-server` accepts only cloud-unaware generality refactors.
- Each account gets a subdomain (`<slug>.atomic.cloud`) which doubles as the
  primary tenant-routing input.
- Self-hosted Atomic continues to work exactly as before, on SQLite, with no
  feature flags or cloud-aware code paths.

## Non-goals (v1)

- Bring-your-own-domain (custom domains pointed at an account). Paid add-on
  later.
- Cross-account features (shared knowledge bases, team workspaces). One
  account = one user for v1.
- Cluster sharding / region selection. One cluster, one region. Capacity
  ceiling around 2-3k accounts before we need to split; `account_databases`
  carries `cluster_id` from day one so the split is mechanical.
- Migration of existing self-hosted databases into cloud. Cloud signups are
  fresh accounts; users can import via the existing export/import flow but
  tokens are always reissued.

## Architectural principles

**Isolation directive.** Cloud code must be as separate as possible from
`atomic-core` and `atomic-server`. Concretely:

- New `crates/atomic-cloud/` holds everything cloud-specific.
- Dependency arrow is one-way: `atomic-cloud → atomic-server → atomic-core`.
- No `#[cfg(feature = "cloud")]` gates in `atomic-core` or `atomic-server`.
- Grepping the string `cloud` in those crates should find nothing.
- Refactors to `atomic-server` are acceptable *only* when they're justifiable
  as pure generality improvements (route registration as a library function,
  request-extension-based resolution). Cloud-driven, but cloud-unaware.

**Two tiers of "database."** Don't conflate them.

- **Tenant database** (`acct_<uuid>`) — one Postgres database per account,
  on the shared cluster. Runs the existing 18 migrations. The boundary for
  isolation, backup, and (eventually) sharding.
- **User-facing knowledge base** — the existing `db_id` column inside a
  tenant database. A single account can still have work-kb, personal-kb,
  etc. The boundary for user-level organization, *not* tenancy.

The administrative boundary is the Postgres database; `db_id` is the user's
organizational tool inside their tenant.

**Subdomain as primary tenant key.** Each account gets a subdomain. The
`Host` header is the first thing the auth middleware reads — host → account →
token. This collapses several MCP/OAuth questions into a single primitive
and gives us free browser-level cross-tenant isolation (cookies, localStorage,
CORS all bound to origin).

## Crate layout

```
crates/
  atomic-core/         unchanged
  atomic-server/       three small generality refactors (see below); cloud-unaware
  atomic-cloud/        new; binary + library; depends on atomic-server
```

The `atomic-cloud` binary composes `atomic-server`'s route registration with
its own middleware, control-plane handle, and account-management routes. The
self-hosted `atomic-server` binary keeps working exactly as before.

## Tenant model

- Each account owns one Postgres database on the shared cluster, named
  `acct_<uuid>` (UUID, not the subdomain — subdomains are renameable, UUIDs
  are not).
- The cluster also hosts the **control plane database** (`atomic_cloud_control`),
  which is separate from any tenant DB.
- Subdomain → account_id is looked up in control plane and cached.
- pgbouncer sits in front of the cluster in transaction-pooling mode so the
  per-account `sqlx::PgPool` instances can be small (e.g. 5 conns max) without
  blowing the cluster's `max_connections`.

### Subdomain rules

- Users pick a vanity slug at signup (3–32 chars, `[a-z0-9-]`).
- Reserved blocklist: `www`, `app`, `api`, `mcp`, `admin`, `support`,
  `status`, `docs`, `blog`, `auth`, `login`, `signup`, plus the usual ~50
  others. Maintain in `atomic-cloud/src/reserved_subdomains.rs`.
- Public enumeration via DNS is accepted as the norm (Slack, Notion, Linear
  all do this).
- Wildcard cert (`*.atomic.cloud`) via Let's Encrypt DNS-01.
- Wildcard A-record (`*.atomic.cloud` → load balancer).
- `app.atomic.cloud` for the marketing site / signup; per-account subdomains
  for the actual product.

## Auth & tenant routing **[drafted]**

### Control plane schema (first cut)

```
accounts            (id, subdomain UNIQUE, email, status, plan,
                     last_active_db_id?, created_at, deleted_at)
account_databases   (account_id, cluster_id, db_name, status, created_at)
cloud_tokens        (hash, account_id, scope, allowed_db_id?, name,
                     created_at, last_used_at, expires_at?, revoked_at?)
sessions            (hash, account_id, created_at, expires_at,
                     ip_first_seen, ua_first_seen)
oauth_clients       (account_id, client_id, client_secret_hash,
                     client_name, redirect_uris, created_at)
oauth_codes         (code_hash, account_id, client_id, code_challenge,
                     redirect_uri, created_at, expires_at, used, token_id)
provider_credentials (account_id, provider, encrypted_key, model_config,
                      created_at, rotated_at)
```

Notes:

- `account_databases.cluster_id` from day one for future shard split.
- `oauth_clients` and `oauth_codes` are per-account in cloud (vs. server-wide
  in self-hosted) — each subdomain has its own OAuth identity.
- `cloud_tokens` is the single source of truth for all tokens (account-scope,
  KB-scope, MCP-scope). No per-tenant `api_tokens` table in cloud.
- `provider_credentials.encrypted_key` — encrypted at rest. Mechanism TBD
  (KMS vs pgcrypto vs sealed-secrets pattern); see provider key custody
  section.

### Token model

- All tokens live in `cloud_tokens` (option A from the deep dive).
- Format: opaque `atm_<random>` with SHA-256 hash stored. The subdomain
  provides account context, so the token itself doesn't need account-encoding.
- Scope enum: `account` (full access), `database` (one `db_id`), `mcp`
  (MCP-issued, typically database-scoped, OAuth-tied).
- Sessions are separate from tokens — different table because their
  lifetimes and revocation UX differ.

### CloudAuth middleware

Order of operations:

1. Read `Host` header → strip base domain → subdomain.
2. Look up `accounts WHERE subdomain = ?` → 404 if not found.
3. Extract bearer token OR session cookie.
4. Verify against `cloud_tokens WHERE account_id = ? AND hash = ?`
   (or `sessions` for cookie path).
5. Build `AuthPrincipal { account_id, scope, allowed_db_id?, source }`.
6. Resolve `Arc<DatabaseManager>` for the account via `AccountCache`.
7. Insert `ResolvedTenant { principal, manager, event_tx }` into request
   extensions.

The middleware is the entire authorization layer. Route handlers see a
`ResolvedTenant`, never a raw token.

### AccountCache

`HashMap<AccountId, Entry>` with idle TTL eviction and a hard cap.

```rust
struct Entry {
    manager: Arc<DatabaseManager>,    // pointing at acct_<uuid>
    event_tx: broadcast::Sender<ServerEvent>,
    last_touched: Instant,
}
```

On miss: look up `account_databases` → connect a fresh `PostgresStorage` to
the tenant's database → wrap in `DatabaseManager` → insert.

Idle TTL number is TBD; rough target 10–30 minutes for v1. Tune from
production data.

### Db extractor change

Today's `Db` extractor (in `atomic-server`) reads from `AppState.manager`.
After the refactor, it reads from request extensions, with `AppState.manager`
as fallback. The refactor is cloud-unaware — it just makes the extractor
generic over where the manager comes from.

The chokepoint check: if `AuthPrincipal.allowed_db_id` is set, the resolved
`db_id` (from `X-Atomic-Database` header or `last_active_db_id`) must
match. Single test asserts this. Without it, a database-scoped MCP token
could read another KB via header override.

### "Active database" concept

Survives but moves into the control plane: `accounts.last_active_db_id`.
Behaviorally identical to today from the user's POV — the frontend can omit
`X-Atomic-Database` and the server picks the user's last-selected KB. Updated
*only* when the user explicitly switches, not on every request, to avoid
making it a hot row.

### Web sessions

Server-stored sessions in `sessions` table; opaque cookie holds the session
hash. Cookie domain is `.atomic.cloud` (note leading dot) so it works across
all subdomains the user visits — needed for cross-account dashboards, account
switcher, etc. `Secure; HttpOnly; SameSite=Lax`.

Login page lives at `app.atomic.cloud/login`. After auth, redirects to
`<chosen_subdomain>.atomic.cloud/`.

### OAuth

Cloud has its own OAuth flow in `atomic-cloud`. We do **not** extend
`atomic-server`'s OAuth handlers with pluggable storage. The flow is
structurally the same (Dynamic Client Registration + Authorization Code +
PKCE) but each endpoint resolves `account_id` from the host before doing
anything. `atomic-server`'s existing OAuth implementation remains untouched
and continues to serve self-hosted.

### MCP token UX

With subdomains, the MCP setup is one piece of information per account:
`https://<slug>.atomic.cloud/mcp`. Claude Desktop's OAuth flow against that
URL produces an MCP-scoped token automatically. Users don't paste tokens
manually.

Open question: do MCP tokens default to account-scope or per-KB? Tracked in
**Open questions** below.

## atomic-server refactors required

Three changes, all cloud-unaware:

1. **Route registration as a library function.** Extract the actix `App`
   wiring into `pub fn configure_routes(cfg: &mut web::ServiceConfig)` that
   `atomic-cloud` can call after wrapping the scope in its own middleware.

2. **`Db` extractor reads from request extensions, falls back to
   `AppState.manager`.** Self-hosted gets a tiny default middleware that
   populates the extension from `AppState.manager` (no behavior change);
   cloud installs its own middleware that populates from `AccountCache`.

3. **`event_tx` becomes injectable via request extensions** with
   `AppState.event_tx` as fallback. Same pattern as #2 for per-account WS
   channels.

None of these mention cloud. They're each defensible as "make atomic-server
more reusable" on their own merits.

## atomic-core changes required

**None**, given the decisions below.

- Provider config moves to an explicit `Option<ProviderConfig>` parameter on
  `AtomicCore::open*` constructors (option a from the deep dive). When
  `Some`, used directly. When `None`, falls back to today's
  "read from settings" behavior. Self-hosted always `None`; cloud always
  `Some`. atomic-core has no idea why.
- Live config update: a single `update_provider_config` method on
  `AtomicCore` that both modes call. Self-hosted writes to settings then
  reloads; cloud reloads from control-plane state.
- The registry-vs-storage settings split (already in `lib.rs`) accommodates
  registry-less mode — cloud's `AtomicCore` simply has no registry attached.
  No change needed.

## Provisioning lifecycle **[drafted]**

### Signup

Synchronous, inline with the HTTP request, capped at 4–8 concurrent in-flight
provisions per process. Happy path ~2–5 seconds. Steps:

1. Validate (email format, subdomain regex `[a-z0-9-]{3,32}`, not reserved).
2. Magic link sent. User clicks → token consumed → flow continues.
3. Atomically claim the subdomain via UNIQUE constraint:
   `INSERT INTO accounts (id, subdomain, email, status='provisioning', ...)`.
   The UNIQUE failure path is what makes "subdomain taken" a race-free check.
4. `CREATE DATABASE acct_<base32(uuid)>` on the cluster.
5. Connect a fresh `PostgresStorage`; call `initialize()` (runs migrations).
6. Seed `databases` row inside the tenant DB: `(id='default', name='Default',
   is_default=true)`.
7. Seed per-DB default settings (wiki prompt template, etc.). Do **not** seed
   provider config — that's in control plane via BYOK.
8. Seed the default Report (per the reports plan).
9. Insert `account_databases (account_id, cluster_id, db_name, status='active')`.
10. Flip `accounts.status='active'`.
11. Create session, set cookie, redirect to `<slug>.atomic.cloud/`.

**Idempotency** — each step is independently idempotent so a crashed signup
can be retried or reaped:

- `SELECT FROM pg_database WHERE datname = ?` before CREATE.
- Migrations are idempotent via `schema_version` + advisory lock.
- Seed inserts use `ON CONFLICT DO NOTHING`.

**No starter atoms** — render an empty-state UI explaining how to capture a
first atom rather than seeding fake content.

**Safety-net reaper** picks up rows stuck in `status='provisioning'` for >5
minutes and either retries or rolls back (DROP DATABASE WITH FORCE if the
database exists, mark `accounts.status='failed'`, free the subdomain).

### Account deletion

Hard delete v1 (no grace period, no soft-delete). User confirms → everything
gone.

1. Revoke all `cloud_tokens` (set `revoked_at`).
2. Invalidate all `sessions`.
3. Evict `AccountCache` entry, drain pool.
4. Terminate stragglers: `SELECT pg_terminate_backend(pid) FROM
   pg_stat_activity WHERE datname = ?` (or rely on `DROP DATABASE WITH FORCE`).
5. `DROP DATABASE ... WITH (FORCE)`.
6. Delete `account_databases` row.
7. Hard-delete `accounts` row.
8. Reserve the subdomain in `subdomains_reserved (subdomain, expires_at =
   now() + 90 days)` to prevent confusion if external clients (RSS readers,
   MCP configs) still point at the old name.

### Schema migration on deploy

The new binary boots in **migrating mode** and doesn't pass readiness until
fleet migration completes. One mechanism, one policy, in one place.

Compile-time `TARGET_SCHEMA_VERSION = N`. On boot:

1. Enumerate `account_databases WHERE status='active' AND
   last_migrated_version < N`.
2. Fan out with concurrency cap (start at 16, tune from production).
3. Per tenant: connect, call `storage.initialize()`, record outcome
   (`last_migrated_version`, `last_migrated_at`, or `migration_failed_at` +
   `last_migration_error`).
4. While migrating: liveness ready, readiness NOT ready.
5. On completion, compute failure rate and apply policy:

| Failure rate | Action |
|---|---|
| 0% | Flip readiness ready. |
| 0 < x < 1% | Flip ready. Stragglers get hold-message; reaper retries. |
| 1% ≤ x < 10% | Stay not-ready. `deploy_status='awaiting_review'`. Operator inspects and either advances or rolls back. |
| x ≥ 10% | Stay not-ready. `deploy_status='rollback_required'`. Migration is broken. |
| Migration runs > 30 min | Stay not-ready. `deploy_status='migration_timeout'`. |

**Rolling deploys** work without coordination because migrations are
**additive-only**: ADD COLUMN, CREATE TABLE, CREATE INDEX, deferred/not-validated
constraints. No DROP COLUMN, no ALTER COLUMN TYPE, no rename. Drops happen
N+1 deploys later, after all referring code is out of the fleet. Enforced by
a custom lint in atomic-cloud's CI that scans migration SQL.

**Stragglers** — when CloudAuth resolves an account with
`last_migrated_version < TARGET_VERSION`, it returns 503:

```json
{ "error": "account_upgrading",
  "message": "Your account is being upgraded. Try again shortly.",
  "retry_after_seconds": 60 }
```

Frontend renders a friendly upgrade screen. MCP clients back off and retry.
The always-running reaper retries failed migrations on a backoff schedule and
alerts when `retry_count > 5`.

**Rollback** is structurally safe with additive-only migrations: rolling
back the binary to version M while some tenants are on schema M+1 means old
code reads extra columns it doesn't know about (ignored). The forward-roll
later is a no-op for already-migrated tenants and a retry for the rest.

### Failure recovery & the reaper

One periodic job, runs every ~60s, takes a control-plane advisory lock keyed
on `account_id` for each row it processes (multiple atomic-cloud processes
can run reapers concurrently):

- Stuck provisioning: `accounts WHERE status='provisioning' AND created_at <
  now() - interval '5 minutes'`.
- Failed migrations: `account_databases WHERE migration_failed_at IS NOT
  NULL AND (migration_retry_after IS NULL OR migration_retry_after <= now())`.
- Anything else: same shape.

Probably the same job runner handles reapers, feed polling, scheduler — see
the worker-fairness deep dive.

## Worker fairness & job queue **[drafted]**

### Shape

A central **dispatcher** in `atomic-cloud`, fed by the existing durable
ledgers (`atom_pipeline_jobs`, `task_runs`) inside tenant DBs, dispatching to
**bounded worker pools per work class** with **per-tenant fairness**. No new
storage primitive; the ledgers stay where they are.

```
DURABLE LEDGERS (inside each tenant DB)
  atom_pipeline_jobs   (per db_id)
  task_runs            (per db_id; reports, scheduled tasks, feed-polls, wiki regen)
        ↓
DISPATCHER (one per atomic-cloud pod, no leader election)
  poll → round-robin per tenant → submit
        ↓
WORKER POOLS (in-memory, per-pod, per class)
  embedding   32 total / 4 per-tenant
  llm         16 total / 2 per-tenant
  ingestion   16 total / 4 per-tenant
  maintenance  8 total / 1 per-tenant
        ↓
Provider calls + tenant DB writes
```

Initial cap numbers are guesses calibrated to ~50 active tenants per pod. Real
numbers come from load testing; ship conservative, raise from metrics.

### Selection algorithm

Plain **round-robin per tenant** within each pool's ready-queue: deque of
per-tenant deques. Pop a tenant, take one job, push the tenant back. Skip
tenants over their per-tenant cap. Drop tenants with empty deques.

Plan-tier weighted fairness is deferred — uniform weights v1, switch to
weighted/DRR when plan tiers exist. Data model needs no preparation; weights
derive from `accounts.plan` when added.

### Cross-tenant ledger scan

Dispatcher uses **N+1 polling with a pending-work hint bit**.

- Application code that enqueues a ledger row also writes to a control-plane
  `dispatch_hints (account_id, last_enqueued_at)` table.
- Dispatcher reads `dispatch_hints` first — only polls tenant DBs that have
  the hint set. Idle tenants are skipped entirely.
- When a tenant's ledger comes back empty, dispatcher clears the hint.
- If a hint write fails (dual-write inconsistency), the work sits in the
  ledger until the next time *someone* enqueues for that tenant; not great
  but bounded. A slow-path full scan every N minutes catches orphans.

Pgbouncer transaction-pooling absorbs the per-tenant connection cost. At
scale (thousands of active tenants per pod), revisit and consider moving to
the full outbox pattern.

### Per-pod, no leader election

Each `atomic-cloud` pod runs its own dispatcher. `FOR UPDATE SKIP LOCKED` on
ledger claims guarantees no double-dispatch. Jittered polling intervals
across pods reduce thundering-herd cost on `dispatch_hints`. Leader election
is the optimization-when-it-hurts answer, not the v1 answer.

### Streaming chat (not in a pool)

Request-driven, user-facing, latency-critical. Per-tenant semaphore at the
route handler (cap = 3 concurrent streams). Provider rate limits do the
actual throttling downstream. No queue involvement.

### Provider rate-limit handling

Two layers:

1. **Local retry with backoff** — worker that hits 429 records the
   rate-limit-reset header into `task_runs.next_attempt_at` or
   `atom_pipeline_jobs.not_before`, releases the lease. Ledger handles
   re-dispatch.
2. **Per-tenant circuit breaker** — 3 consecutive 429s in 60s pauses that
   tenant's dispatch for a cool-down (60s, doubling). State lives in
   `accounts.provider_paused_until`. Also handles "BYOK key expired" — the
   breaker stays open until the user fixes it.

### How each work-type lands

| Work type | Today | Cloud |
|---|---|---|
| Embedding/tagging | `atom_pipeline_jobs` ledger + spawn | Same ledger; dispatcher → embedding pool |
| Wiki regen | Fire-and-forget on tag change | New `task_runs` entry `wiki.regenerate`; LLM pool |
| Reports | `task_runs` (already) | Same; LLM pool, per-tenant cap 1 |
| Feeds | 60s loop, special-case | **Move to `task_runs`**; ingestion pool |
| DraftPipelineTask, GraphMaintenanceTask | 15s loop with lock map | **Move to `task_runs`**; maintenance pool |
| Streaming chat | Handler streams provider | Same, with per-tenant route-handler semaphore (cap 3) |
| Canvas warmup | One-shot on boot | Lazy; "all-tenants boot" doesn't apply in multi-tenant |

The "move to `task_runs`" rows are a separate workstream (see below).

### Restart semantics

Standard. Pod restart drops in-memory ready-queues. Durable ledgers re-claim
expired-lease jobs. In-flight streaming chats terminate; frontend retries.

## `task_runs` unification (cross-cutting workstream)

Moving feed polling, `DraftPipelineTask`, `GraphMaintenanceTask`, and wiki
regen into `task_runs` is a refactor of `atomic-core` (where task definitions
live) and `atomic-server` (which dispatches them today). It is **not
cloud-specific** — `task_runs` was designed for this from the start
(see comment in migration 015 referring to phase 1.5's dormant-helper
ship). Self-hosted benefits from the unification too: one durable ledger
with the existing claim/lease/crash-recovery semantics replaces several
ad-hoc loops.

This deserves its own plan doc (`docs/plans/task-runs-unification.md`) and a
separate workstream. Its outputs feed atomic-cloud's dispatcher but the work
itself lives in atomic-core/atomic-server, cleanly. Atomic-cloud just relies
on `task_runs` being the single source of pending work.

Sequencing-wise: unification can land first, before atomic-cloud exists, and
ride to production in self-hosted. By the time atomic-cloud's dispatcher is
built, all background work is already going through one ledger.

## Provider key custody **[drafted]**

BYOK only for v1 (decided earlier). Each tenant provides their own OpenRouter
or OpenAI-compatible key. Platform-proxy with metering is a future paid-tier.
Ollama is **not supported in cloud** — local-only by definition.

### Storage schema

```sql
provider_credentials (
    account_id              TEXT NOT NULL,
    provider                TEXT NOT NULL,   -- 'openrouter' | 'openai_compat'
    encrypted_key           BYTEA NOT NULL,
    nonce                   BYTEA NOT NULL,  -- 96-bit, fresh per encryption
    encryption_version      INT  NOT NULL,   -- master-key generation
    model_config            JSONB NOT NULL,  -- { embedding_model, llm_model, ... }
    created_at              TIMESTAMPTZ NOT NULL,
    rotated_at              TIMESTAMPTZ,
    last_used_at            TIMESTAMPTZ,
    last_validated_at       TIMESTAMPTZ,
    last_validation_error   TEXT,
    PRIMARY KEY (account_id, provider)
)
```

`accounts.active_provider` selects which row is the active config. Composite
PK on `(account_id, provider)` allows a user to keep multiple providers
configured (e.g., OpenRouter for general use + a self-hosted OpenAI-compatible
endpoint) and switch.

Model selection (`model_config`) lives **with the key** in control plane, not
in per-DB settings. Rationale: provider config is account-level — different
KBs sharing one account using different models is more flexibility than users
want. Per-DB override remains an optional future feature.

### Encryption at rest

Wrapped behind a `KeyVault` trait in atomic-cloud with two methods:

```rust
trait KeyVault {
    fn encrypt(&self, account_id: &str, provider: &str, plaintext: &[u8])
        -> Result<(Vec<u8>, Vec<u8>, i32)>;  // ciphertext, nonce, version
    fn decrypt(&self, account_id: &str, provider: &str, ct: &[u8], nonce: &[u8],
        version: i32) -> Result<Vec<u8>>;
}
```

**v1 implementation `EnvMasterKeyVault`**: AES-256-GCM with 32-byte master key
loaded from env at process start. Fresh nonce per row. AAD = `account_id ||
provider`, binding ciphertext to its row.

**v2 implementation `KmsEnvelopeVault`**: per-account DEKs encrypted by a KMS
master key. DEK ciphertext stored alongside `encrypted_key`. Cached in
AccountCache so KMS calls amortize across requests. Same schema; swap is
contained.

Master key rotation in v1: bump `encryption_version`, lazy re-encrypt on
next access. Master key custody: sealed-secret at deploy, backed up
out-of-band. **Loss of master key = unrecoverable keys.** Document
explicitly in operator runbook.

### Key entry & validation

- Signup flow has an optional "Add provider key" step. Skippable.
- Settings page at `<slug>.atomic.cloud/settings/provider` for post-signup
  entry/rotation.
- **Existing key is never displayed.** Status only ("configured ✓, last
  validated 3h ago"). Rotation = replace.
- **Validation on save** — test call against the provider before storing:
  OpenRouter `GET /api/v1/auth/key`; OpenAI-compat minimal embedding call.
  Failure surfaces provider's error verbatim, rejects the save.
- **Periodic re-validation** — deferred. See Open questions.

### Empty-state (no key configured)

- Atoms create/update fine.
- Embedding pipeline jobs sit in the ledger with `state='blocked_on_provider'`;
  not dispatched until a key is configured.
- Wiki regen, reports, semantic search, chat return a structured
  "configure your AI provider" error.
- Frontend banner directs to the settings page.

### Plumbing — control plane → AtomicCore

On AccountCache miss:

1. Load `provider_credentials` row for the account's active provider.
2. `KeyVault::decrypt(...)` → plaintext key.
3. Build `ProviderConfig` from row + decrypted key.
4. `AtomicCore::open_postgres(cluster_url, "acct_<id>", "default",
   Some(provider_config))`.

Cloud always passes `Some(ProviderConfig)` — never `None` (which would route
through atomic-core's settings-table fallback). If no row exists, pass a
`ProviderConfig` with `*_api_key: None` — atomic-core builds providers in
"missing key" state that reject calls with a structured error.

### Live rotation

1. Validate new key.
2. UPSERT `provider_credentials` (bump `rotated_at`).
3. Build fresh `ProviderConfig`.
4. Look up the AccountCache entry, call `core.update_provider_config(new)`.
5. In-flight requests using the old config complete; new requests use the new.
6. **Clear `accounts.provider_paused_until`** if circuit-breaker was open —
   new key deserves a fresh chance.

### In-process hygiene

- **Custom `Debug` impl on `ProviderConfig` redacts `*_api_key` fields.**
  This change lives in `atomic-core`, not atomic-cloud — it's pure hygiene,
  useful for self-hosted logging too, and not cloud-aware.
- Never include the key in error messages, traces, or logs. Audit
  instrumentation around provider calls.
- No "zeroize on drop" — overkill for our threat model. Standard Rust drop
  semantics free the key.

### Audit / visibility in settings UI

- Provider, configured ✓ (no value).
- Last validated, last used.
- Current status (Healthy / Paused / Failing).
- Recent errors (timestamp + redacted message).

### Trust-building docs (launch task)

Customer-facing "where does my key go?" page explaining encryption,
in-process decryption, no-logging discipline. B2B norm. Not architectural,
but write it for launch.

## Observability, quotas, billing **[drafted]**

### Observability

Three audiences (operator, user, support); four data kinds.

**Metrics.** Split per-tenant (small set, operationally critical) from
per-cluster (everything else) to bound cardinality.

| Metric | Cardinality |
|---|---|
| `requests_per_account_total{account_id}` | Per-tenant |
| `provider_errors_per_account_total{account_id, provider, code}` | Per-tenant |
| `queue_depth_per_account{account_id, pool}` | Per-tenant |
| `http_request_duration_seconds{route, status}` | Per-cluster |
| `worker_pool_in_flight{pool}` | Per-cluster |
| `account_cache_size` | Per-cluster |

Higher-cardinality detail (per-tenant latency) defers to traces.

**Structured logs.** Every route, worker job, and provider call emits
JSON with `account_id`, `db_id`, `request_id`. Discipline: route handlers
never call `tracing::info!()` directly — they use a helper that injects
account context from the request. Clippy lint enforces.

**Tracing.** OpenTelemetry with `account_id` as a root-span attribute
propagated down. Head sampling (1–5%) baseline; tail sampling for errors
if our backend supports.

**Per-account event log (user-facing)** — distinct from system logs. Schema
in tenant DB:

```sql
account_events (
    id BIGSERIAL PRIMARY KEY,
    db_id TEXT NOT NULL,
    occurred_at TIMESTAMPTZ NOT NULL,
    event_type TEXT NOT NULL,
    subject_id TEXT,
    metadata JSONB
)
```

Rows only for discrete, named outcomes the user cares about (atom creation,
report run, wiki regen, provider failure). High-volume operations
(embedding chunks) stay in logs and roll up into daily counters in
`quota_usage`. TTL via partitioned tables (90 days default).

### Quotas

Two categories with very different consistency requirements:

**Anti-abuse rate limits** — sliding-window per-pod counters via `governor`,
keyed by account_id. Approximate consistency is fine. Defaults:

| Limit | Window | Default |
|---|---|---|
| API requests | per min | 600 |
| Signup attempts | per IP per hour | 5 |
| Magic link requests | per email per hour | 3 |
| URL ingestion | per min | 30 |
| Atom creates | per min | 60 |

**Plan-tier resource limits** — strong consistency via Postgres UPSERT.

```sql
plans (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    monthly_price_cents INT,
    atom_limit INT,                       -- NULL = unlimited
    llm_calls_monthly_limit INT,
    kb_limit INT,
    storage_bytes_limit BIGINT,
    feature_flags JSONB
)

quota_usage (
    account_id TEXT NOT NULL,
    period_start DATE NOT NULL,
    metric TEXT NOT NULL,
    value BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (account_id, period_start, metric)
)
```

Plan-tier configuration in code or a small seeded table. `accounts.plan_id`
references it.

**Enforcement points:**

| Where | Check | Action on hit |
|---|---|---|
| CloudAuth middleware | Rate limit | 429 with `Retry-After` |
| Atom create | `atoms_count < limit` | 402 with quota error |
| KB create | `kb_count < limit` | 402 with quota error |
| Provider call site | `llm_calls < limit` | 402; background jobs **block** (not fail) |
| Periodic reaper | Storage bytes recompute | Week 1 warn; week 2 restrict writes; **no auto-delete** |

Quota-exceeded response shape:

```json
{ "error": "quota_exceeded",
  "metric": "llm_calls",
  "current": 5000,
  "limit": 5000,
  "resets_at": "2026-06-01T00:00:00Z",
  "upgrade_url": "https://app.atomic.cloud/billing" }
```

Background jobs that hit LLM limits **sit in the ledger** (not fail) until
quota resets or user upgrades. Same hold-message pattern as account-upgrading.

Period rollover: 1-hour-cadence job inserts new `period_start` rows when due.
Old rows kept for billing/audit.

### Billing

**v1 model:** BYOK + subscription. User pays for platform (hosting, features).
AI costs go to OpenRouter directly via their BYOK key. No AI-call billing,
no margin risk. Platform-proxy with per-call metering is v2.

```sql
stripe_customers (
    account_id TEXT PRIMARY KEY,
    stripe_customer_id TEXT UNIQUE NOT NULL,
    default_payment_method_id TEXT,
    created_at TIMESTAMPTZ
)

stripe_subscriptions (
    account_id TEXT PRIMARY KEY,
    stripe_subscription_id TEXT UNIQUE NOT NULL,
    plan_id TEXT NOT NULL,
    status TEXT NOT NULL,
    current_period_start TIMESTAMPTZ NOT NULL,
    current_period_end TIMESTAMPTZ NOT NULL,
    cancel_at_period_end BOOLEAN NOT NULL DEFAULT false,
    updated_at TIMESTAMPTZ NOT NULL
)
```

- "Manage billing" → Stripe Customer Portal (Stripe owns the UI for invoices,
  payment methods, plan changes).
- Webhook at `app.atomic.cloud/billing/webhook` (single URL, not
  per-subdomain). Verifies Stripe signature, updates rows.
- Key events: `customer.subscription.{created,updated,deleted}`,
  `invoice.payment_{succeeded,failed}`.

**Plan transitions:**

| Trigger | Effect |
|---|---|
| Checkout success | `accounts.plan_id` updated, quotas widen |
| Downgrade | Plan updated. Over-limit usage retained but writes blocked until under. No auto-deletion. |
| Upgrade | Plan updated immediately, Stripe handles proration |
| Payment fail (Stripe dunning x3 over 1 week) | Status → `past_due` |
| 3 days past_due | Read-only mode |
| 14 days past_due | Suspended (login blocked), data retained |
| Subscription deleted | Drops to free plan; if over free limits, read-only until under |

**Never auto-delete data for payment failure.** Hard-delete only on explicit
user action. Right ethically and commercially (re-conversion is real revenue).

**Free tier (defaults, product-tunable):** 100 atoms, 50 LLM calls/mo, 1 KB,
100 MB storage. All features available — no feature-gated free tier.

**Trials:** 14 days of paid tier on signup, **no card required**. Auto-
downgrade to free after. Accepts signup-spam risk for friction-free
onboarding; magic-link + rate-limited signup bounds the abuse vector.

## Open questions (carried across sections)

- **Account signup mechanism.** Email/password vs magic-link vs OAuth IdP
  (GitHub etc.). Magic-link likely simplest to operate; email deliverability
  becomes critical-path.
- **Free tier shape & abuse model.** Open free signup needs CAPTCHA +
  rate-limited token issuance. Invite-only or paid-from-day-one is much
  simpler.
- **MCP token default scope.** Account-wide vs per-KB. Affects the MCP setup
  UX in Claude Desktop's config.
- **AccountCache idle-TTL and hard-cap numbers.** Tune from real load; initial
  guess 10–30 min TTL, cap at 1000 entries.
- **Provider key custody model** (the whole [stub] above).
- **Periodic provider-key re-validation.** Reaper-driven daily check
  (capped, skipped for active keys) catches quietly-expired keys before
  users hit them through failed work. Costs one test call per active key per
  day against the user's quota; adds reaper complexity. Decide once we see
  how often keys quietly expire in practice.
- **Per-tenant metric cardinality strategy.** Top-N high-cardinality buckets
  + aggregate vs buy a high-cardinality TSDB (Mimir, VictoriaMetrics).
  Deferable until we have noisy tenants.
- **Plan tier structure beyond free.** Number of paid tiers, what features
  differ, pricing. Product/business call.
- **Free-tier limits (numbers).** Placeholder is 100 atoms / 50 LLM calls /
  1 KB / 100 MB.
- **Storage quota unit — bytes vs atoms.** Bytes is more accurate for cost,
  atoms is easier to communicate. Likely bytes for enforcement, atoms for
  marketing copy.
- **Trial length.** 14 days conventional; 7 or 30 also defensible.
- **Read-only / suspended UX details.** What the user sees, friendly upgrade
  prompts, "your data is safe" messaging.
- **account_events retention policy.** 90 days default? Per-event-type
  retention?
- **Tracing sample rate.** 1–5% baseline; tail sampling for errors.

## Decisions log

Capture choices we've already made so we don't relitigate. Date each entry
and link the discussion if it lives in a memory file.

- **2026-05-25** — Cloud uses Postgres, not SQLite. Database-per-tenant on
  shared cluster. Supersedes the earlier SQLite + per-customer mounted
  volume plan.
- **2026-05-25** — Hard isolation directive: cloud code lives in
  `crates/atomic-cloud/`, no cloud-aware code in atomic-core or
  atomic-server.
- **2026-05-25** — Per-account subdomains as primary tenant routing input.
  Vanity slugs at signup, public enumeration via DNS accepted.
- **2026-05-25** — All tokens in control plane (option A). Token format is
  opaque `atm_<random>`; subdomain provides account context.
- **2026-05-25** — Server-stored web sessions, cookie scoped to
  `.atomic.cloud`.
- **2026-05-25** — Separate OAuth implementation in atomic-cloud; do not
  extend atomic-server's OAuth handlers with pluggable storage.
- **2026-05-25** — Provider config via explicit `Option<ProviderConfig>`
  parameter on `AtomicCore::open*` (option a). No traits; atomic-core gains
  no cloud awareness.
- **2026-05-25** — Three atomic-server generality refactors approved: route
  registration as library function, request-extension-based core resolution,
  request-extension-based event channel.
- **2026-05-25** — "Active database" concept moves to per-account state
  (`accounts.last_active_db_id`) rather than being killed.
- **2026-05-25** — `account_databases` carries `cluster_id` from day one so
  future shard split is mechanical.
- **2026-05-25** — Signup is synchronous (inline with HTTP request),
  capped at 4–8 concurrent provisions per process. Safety-net reaper for
  stuck rows.
- **2026-05-25** — Hard delete for v1. No grace period, no soft-delete.
  Freed subdomains reserved for 90 days before reuse.
- **2026-05-25** — Tenant database naming: `acct_<base32(uuid)>`. Opaque,
  fixed-length, doesn't leak tenant count.
- **2026-05-25** — Custom-domain support (BYOD) is the next subdomain-adjacent
  feature after v1 ships. Subdomain renaming is deferred indefinitely; we go
  straight from "subdomain only" to "subdomain + custom domain."
- **2026-05-25** — Seed defaults: default KB (`db_id='default'`), per-DB
  default settings, default Report (per reports plan). No starter atoms;
  empty-state UI instead.
- **2026-05-25** — BYOK for provider keys in v1. Platform-proxy is a future
  paid-tier addition.
- **2026-05-25** — Authentication is **magic-link only**. No password
  infrastructure. Email verification falls out of signup naturally — clicking
  the link proves email ownership.
- **2026-05-25** — Deploy gating runs inside the new binary's boot sequence:
  fleet migration completes before readiness flips ready. Thresholds:
  <1% failures = proceed; 1–10% = await review; ≥10% = rollback required;
  >30min wall time = timeout. Stragglers get a 503 `account_upgrading`
  response; reaper retries them.
- **2026-05-25** — Migrations are **additive-only**. ADD COLUMN, CREATE
  TABLE, CREATE INDEX, deferred constraints. No DROP COLUMN, no ALTER COLUMN
  TYPE, no rename. Drops happen N+1 deploys after all referring code is gone.
  Enforced by a CI lint on migration SQL.
- **2026-05-25** — Central dispatcher in atomic-cloud, per-pod, no leader
  election. `FOR UPDATE SKIP LOCKED` on ledger claims. Round-robin per-tenant
  fairness (uniform weights v1; plan-tier weighting deferred).
- **2026-05-25** — Four worker pools per work class (embedding / llm /
  ingestion / maintenance) with total and per-tenant in-flight caps.
  Streaming chat is not in a pool — per-tenant semaphore at the route handler.
- **2026-05-25** — Cross-tenant ledger scan via N+1 polling + control-plane
  `dispatch_hints` bit. Outbox pattern is deferred until N+1 hurts.
- **2026-05-25** — Per-tenant provider circuit breaker (3×429 in 60s → 60s
  cool-down, doubling). State in `accounts.provider_paused_until`.
- **2026-05-25** — Unify feed polling, DraftPipelineTask, GraphMaintenanceTask,
  and wiki regen onto `task_runs`. Cloud-driven but **not cloud-specific** —
  the refactor lives in atomic-core/atomic-server and benefits self-hosted.
  Gets its own plan doc (`docs/plans/task-runs-unification.md`) and a
  separate workstream that can land before atomic-cloud exists.
- **2026-05-25** — Provider keys encrypted at rest via app-side AES-256-GCM
  with master key in env (v1). Wrapped behind a `KeyVault` trait so KMS
  envelope encryption is a localized swap (v2). Schema doesn't change.
- **2026-05-25** — `model_config` lives with the key in control plane, not in
  per-DB settings. Provider config is account-level v1; per-KB override is a
  future optional addition.
- **2026-05-25** — Cloud does **not** support Ollama. OpenRouter and
  OpenAI-compatible only.
- **2026-05-25** — Validate provider keys on save (test call against
  provider's auth-check endpoint). Periodic re-validation deferred — see
  Open questions.
- **2026-05-25** — Cloud always passes `Some(ProviderConfig)` to
  `AtomicCore::open*` — `None` would fall back to settings-table lookup,
  which is the registry-fallback path we explicitly avoid in cloud.
- **2026-05-25** — `ProviderConfig` gets a custom `Debug` impl that redacts
  `*_api_key` fields. Lives in atomic-core (not cloud-aware; pure hygiene).
- **2026-05-25** — Observability: per-tenant labels only on a small
  operationally-critical metric set; everything else per-cluster. Higher-
  cardinality detail goes through traces, not metrics.
- **2026-05-25** — `account_events` table in tenant DB for user-facing
  activity log. Discrete named outcomes only; high-volume operations stay
  in logs + rollups in `quota_usage`.
- **2026-05-25** — Two-tier quotas: anti-abuse rate limits (per-pod
  approximate counters via `governor`) and plan-tier resource limits
  (Postgres-backed strong consistency via UPSERT on `quota_usage`).
- **2026-05-25** — Background jobs that hit LLM quota limits **block**
  (sit in ledger) rather than fail. Same hold-message pattern as
  account-upgrading.
- **2026-05-25** — Billing v1 is BYOK + subscription (platform fees only,
  no AI-call metering). Platform-proxy with per-call metering is v2.
- **2026-05-25** — Stripe via Customer Portal. Webhook at
  `app.atomic.cloud/billing/webhook` (single URL, not per-subdomain).
- **2026-05-25** — **Never auto-delete data for payment failure.** Read-only
  after 3 days past_due, suspended after 14 days, data retained. Hard-delete
  only on explicit user action.
- **2026-05-25** — Trials: 14 days of paid tier on signup, no card required.
  Auto-downgrade to free after.

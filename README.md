# evgl-api

**Evento Globolo — Rust REST and WebSocket API server**

A global events operating system combining event discovery, publishing, RSVP, ticketing, community, venue, and organizer workflows.

This repository was bootstrapped on 2026-08-04. It is designed as an independently deployable component and as a member of the `evgl-monorepo` workspace.

## GitHub target

`evento-globolo/evgl-api`

## Baseline

- Rust 2024 edition for backend and native components.
- Axum HTTP/WebSocket transport.
- Supabase/PostgreSQL configuration through `DATABASE_URL`, `SUPABASE_URL`, and environment-only secrets.
- OpenTelemetry-compatible tracing hooks.
- Docker, Nix, and GitHub Actions entry points.
- Contracts live in `evgl-interfaces`; shared behavior lives in `evgl-libs`.

### Routes

- `/v1/events`
- `/v1/events/search`
- `/v1/organizers`
- `/v1/venues`
- `/v1/ws`

## Development

```bash
cp .env.example .env 2>/dev/null || true
nix develop  # optional
cargo fmt --check 2>/dev/null || true
cargo test 2>/dev/null || true
```

### Ticketing and offline admission

`TicketingService` applies the canonical, pinned `evgl-interfaces` inventory
migration and exposes transaction-safe holds, checkout/payment idempotency,
expiry, cancellation/refund, fair waitlist promotion, and aggregate receipts.
`AdmissionService` applies the dependent admission migration and persists
entitlements, public verification keys, signed scanner receipts, revocation
epochs, and deterministic admission decisions. `AdmissionTokenSigner` and
`verify_admission_token` implement bounded-window Ed25519 QR tokens; scanner
receipts use the same canonical-signature discipline.

The PostgreSQL integration tests create isolated schemas and destroy only those
schemas after each run:

```bash
EVGL_TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres \
  cargo test --all-targets --all-features
```

CI provides PostgreSQL 18 for the same concurrency, retry, timeout, refund,
waitlist, offline replay, key-rotation, reconciliation, and revocation canaries.
HTTP route exposure is gated on integration with the event/organizer boundary
in PR #9; this branch deliberately does not modify its `src/main.rs` handler.

## Status

Foundation scaffold. Domain behavior, persistence migrations, authentication policy, and production secrets must be reviewed before deployment.

## Environment secrets

Secrets live in this repo **encrypted** with [sops](https://github.com/getsops/sops) + [age](https://github.com/FiloSottile/age):
`env/enc/<dev|prod>.env.enc` is committed; `just env-use <name>` decrypts it to
`env/dec/<name>.env` (gitignored, mode 0600) and symlinks `./.env` to it. The
Nix dev shell provides the tooling, `just env-audit` runs keyless in CI, and
containers decrypt at `docker run` — never at build. See [`env/README.md`](env/README.md).

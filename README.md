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

## Status

Foundation scaffold. Domain behavior, persistence migrations, authentication policy, and production secrets must be reviewed before deployment.

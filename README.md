# evgl-api

Axum REST and WebSocket API server for Evento Globolo.

**Product:** Evento Globolo — A global event discovery and aggregation platform.

Aggregate, normalize, deduplicate, search, and follow events from sources such as Eventbrite, Meetup, LinkedIn, Facebook, and Craigslist through authorized APIs or permitted ingestion paths.

## Safety and production boundary

Provider names are integration targets, not claims of affiliation. Use official APIs and permitted data-access methods; do not bypass authentication, anti-bot, rate-limit, copyright, or platform-policy controls.

This repository is an executable bootstrap, not a production deployment. Before live
use, add authentication, tenant authorization, rate limits, durable migrations,
observability, backups, incident response, dependency review, and secret management.
## Routes

- `GET /healthz`, `GET /readyz`, `GET /metrics`
- `GET|POST /api/v1/events`
- `GET /api/v1/events/{id}`
- `GET /ws` for JSON event envelopes

The bootstrap uses bounded in-memory state so transport behavior is immediately
testable. Replace it with SeaORM/PostgreSQL transactions before production and keep
`evgl-interfaces` as the tagged wire-contract authority.

```bash
cargo run
```

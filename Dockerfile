# syntax=docker/dockerfile:1
FROM rust:1-slim AS build
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/evgl-api /usr/local/bin/app
ENV HOST=0.0.0.0 PORT=8080
EXPOSE 8080

# --- sops: decrypt at `docker run`, never at `docker build` ------------------
# The image carries only CIPHERTEXT (env/enc/<SOPS_ENV>.env.enc) and the sops
# binary. The age key arrives at run time (SOPS_AGE_KEY / SOPS_AGE_KEY_FILE);
# scripts/sops-entrypoint.sh decrypts into the process environment and execs
# the real command, so no plaintext ever lands in a layer or on disk.
# See env/README.md.
ARG SOPS_ENV=prod
COPY --chmod=0755 --from=ghcr.io/getsops/sops:v3.10.2-alpine /usr/local/bin/sops /usr/local/bin/sops
COPY --chmod=0755 scripts/sops-entrypoint.sh /usr/local/bin/sops-entrypoint.sh
COPY --chmod=0644 env/enc/${SOPS_ENV}.env.enc /app/secrets/app.env
ENV SOPS_SECRETS_FILE=/app/secrets/app.env

ENTRYPOINT ["/usr/local/bin/sops-entrypoint.sh"]
# ores-otel: in-process OTLP to the cluster collector. The *-sidecar.rs image is a separate loopback helper on 127.0.0.1:9090 — do not EXPOSE 4317/4318 or 9090.
ENV OTEL_SERVICE_NAME=evgl-api \
    OTEL_EXPORTER_OTLP_ENDPOINT=http://dd-otel-collector.observability.svc.cluster.local:4318 \
    RUST_LOG=info
CMD ["/usr/local/bin/app"]

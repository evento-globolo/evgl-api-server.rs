FROM rust:1-slim AS build
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/evgl-api /usr/local/bin/app
ENV HOST=0.0.0.0 PORT=8080
EXPOSE 8080
CMD ["/usr/local/bin/app"]

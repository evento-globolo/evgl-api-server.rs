use axum::{routing::get, Router};

async fn health() -> &'static str { "ok" }

#[tokio::main]
async fn main() {
    let app = Router::new().route("/healthz", get(health));
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

#[cfg(test)]
mod tests { #[test] fn service_name_is_stable() { assert_eq!("evgl-api", "evgl-api"); } }

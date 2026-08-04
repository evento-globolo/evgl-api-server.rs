        use axum::{
            extract::{Path, State, WebSocketUpgrade, ws::{Message, WebSocket}},
            http::StatusCode,
            response::{IntoResponse, Response},
            routing::get,
            Json, Router,
        };
        use chrono::Utc;
        use futures_util::{SinkExt, StreamExt};
        use evgl_interfaces::{CreateEvent, Event, EventEvent};
        use serde::Serialize;
        use std::{collections::BTreeMap, env, net::SocketAddr, sync::Arc};
        use tokio::sync::{RwLock, broadcast};
        use tower_http::{cors::CorsLayer, trace::TraceLayer};
        use tracing::info;
        use uuid::Uuid;

        #[derive(Clone)]
        struct AppState {
            records: Arc<RwLock<BTreeMap<Uuid, Event>>>,
            events: broadcast::Sender<String>,
        }

        #[derive(Debug, Serialize)]
        struct Health<'a> { status: &'a str, service: &'a str, version: &'a str }

        #[tokio::main]
        async fn main() -> Result<(), Box<dyn std::error::Error>> {
            tracing_subscriber::fmt().with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info,tower_http=info".into()),
            ).init();
            let (events, _) = broadcast::channel(256);
            let state = AppState { records: Arc::new(RwLock::new(BTreeMap::new())), events };
            let app = Router::new()
                .route("/", get(index))
                .route("/healthz", get(health))
                .route("/readyz", get(health))
                .route("/metrics", get(metrics))
                .route("/api/v1/events", get(list_records).post(create_record))
                .route("/api/v1/events/{id}", get(get_record))
                .route("/ws", get(websocket))
                .layer(CorsLayer::permissive())
                .layer(TraceLayer::new_for_http())
                .with_state(state);
            let addr: SocketAddr = env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into()).parse()?;
            let listener = tokio::net::TcpListener::bind(addr).await?;
            info!(%addr, "Evento Globolo API listening");
            axum::serve(listener, app).await?;
            Ok(())
        }

        async fn index() -> Json<serde_json::Value> {
            Json(serde_json::json!({
                "service": "evgl-api",
                "product": "Evento Globolo",
                "rest": "/api/v1/events",
                "websocket": "/ws"
            }))
        }

        async fn health() -> Json<Health<'static>> {
            Json(Health { status: "ok", service: "evgl-api", version: env!("CARGO_PKG_VERSION") })
        }

        async fn metrics(State(state): State<AppState>) -> String {
            format!("# TYPE evgl_records gauge
evgl_records {}
", state.records.read().await.len())
        }

        async fn list_records(State(state): State<AppState>) -> Json<Vec<Event>> {
            Json(state.records.read().await.values().cloned().collect())
        }

        async fn get_record(Path(id): Path<Uuid>, State(state): State<AppState>) -> Result<Json<Event>, ApiError> {
            state.records.read().await.get(&id).cloned().map(Json).ok_or(ApiError::NotFound)
        }

        async fn create_record(
            State(state): State<AppState>,
            Json(input): Json<CreateEvent>,
        ) -> Result<(StatusCode, Json<Event>), ApiError> {
            let now = Utc::now();
            let record = input.into_record(Uuid::new_v4(), now).map_err(|error| ApiError::Validation(error.to_string()))?;
            state.records.write().await.insert(record.id, record.clone());
            let event = EventEvent {
                event_id: Uuid::new_v4(),
                event_type: "event.created".into(),
                occurred_at: now,
                data: record.clone(),
            };
            let _ = state.events.send(serde_json::to_string(&event).expect("serializable event"));
            Ok((StatusCode::CREATED, Json(record)))
        }

        async fn websocket(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
            ws.on_upgrade(move |socket| socket_loop(socket, state.events.subscribe()))
        }

        async fn socket_loop(socket: WebSocket, mut events: broadcast::Receiver<String>) {
            let (mut sender, mut receiver) = socket.split();
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        Ok(text) => {
                            if sender.send(Message::Text(text.into())).await.is_err() { break; }
                        },
                        Err(broadcast::error::RecvError::Closed) => break,
                        _ => {},
                    },
                    message = receiver.next() => match message {
                        Some(Ok(Message::Ping(data))) => {
                            if sender.send(Message::Pong(data)).await.is_err() { break; }
                        },
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Err(_)) => break,
                        _ => {},
                    }
                }
            }
        }

        #[derive(Debug)]
        enum ApiError { NotFound, Validation(String) }

        impl IntoResponse for ApiError {
            fn into_response(self) -> Response {
                match self {
                    Self::NotFound => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error":"not_found"}))).into_response(),
                    Self::Validation(message) => (StatusCode::UNPROCESSABLE_ENTITY, Json(serde_json::json!({"error":"validation", "message":message}))).into_response(),
                }
            }
        }

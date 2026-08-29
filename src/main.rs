mod flags;

use std::{collections::HashMap, sync::Arc};

use anyhow::Context;
use axum::{
    Json, Router,
    extract::{
        Path, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use sea_orm::{Database, DatabaseConnection};
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, broadcast};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::info;
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    db: Option<DatabaseConnection>,
    records: Arc<RwLock<HashMap<Uuid, Event>>>,
    events: broadcast::Sender<String>,
    supabase_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Event {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub title: String,
    pub description: String,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub venue: String,
    pub city: String,
    pub country: String,
    pub organizer_id: Uuid,
    pub capacity: i32,
}

#[derive(Debug, Deserialize)]
struct CreateEvent {
    pub title: String,
    pub description: String,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub venue: String,
    pub city: String,
    pub country: String,
    pub organizer_id: Uuid,
    pub capacity: i32,
}

#[derive(Debug, Serialize)]
struct Health {
    service: &'static str,
    status: &'static str,
    database_configured: bool,
    supabase_configured: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Some(output) = flags::process_control().map_err(anyhow::Error::msg)? {
        print!("{output}");
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_new(
            flags::var("RUST_LOG").unwrap_or_else(|_| "error".to_owned()),
        )?)
        .init();

    let db = match flags::var("DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => {
            Some(Database::connect(url).await.context("connect database")?)
        }
        _ => None,
    };
    let (events, _) = broadcast::channel(512);
    let state = AppState {
        db,
        records: Arc::new(RwLock::new(HashMap::new())),
        events,
        supabase_url: flags::var("SUPABASE_URL").ok(),
    };

    let app = Router::new()
        .route("/healthz", get(health))
        .route("/v1/events", get(list_records).post(create_record))
        .route("/v1/events/{id}", get(get_record))
        .route("/v1/ws", get(ws_upgrade))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let host = flags::var("HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let port = flags::var("PORT").unwrap_or_else(|_| "8080".into());
    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}")).await?;
    info!(address = %listener.local_addr()?, "Evento Globolo API listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn health(State(state): State<AppState>) -> Json<Health> {
    Json(Health {
        service: "evgl-api",
        status: "ok",
        database_configured: state.db.is_some(),
        supabase_configured: state.supabase_url.is_some(),
    })
}

async fn list_records(State(state): State<AppState>) -> Json<Vec<Event>> {
    Json(state.records.read().await.values().cloned().collect())
}

async fn get_record(Path(id): Path<Uuid>, State(state): State<AppState>) -> impl IntoResponse {
    match state.records.read().await.get(&id).cloned() {
        Some(record) => (StatusCode::OK, Json(record)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn create_record(
    State(state): State<AppState>,
    Json(input): Json<CreateEvent>,
) -> impl IntoResponse {
    let now = Utc::now();
    let record = Event {
        id: Uuid::new_v4(),
        created_at: now,
        updated_at: now,
        title: input.title,
        description: input.description,
        starts_at: input.starts_at,
        ends_at: input.ends_at,
        venue: input.venue,
        city: input.city,
        country: input.country,
        organizer_id: input.organizer_id,
        capacity: input.capacity,
    };
    state
        .records
        .write()
        .await
        .insert(record.id, record.clone());
    let _ = state
        .events
        .send(serde_json::to_string(&record).unwrap_or_default());
    (StatusCode::CREATED, Json(record))
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| websocket(socket, state))
}

async fn websocket(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let mut events = state.events.subscribe();
    let send_task = tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            if sender.send(Message::Text(event.into())).await.is_err() {
                break;
            }
        }
    });
    let receive_task = tokio::spawn(async move {
        while let Some(Ok(message)) = receiver.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    });
    tokio::select! { _ = send_task => {}, _ = receive_task => {} }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

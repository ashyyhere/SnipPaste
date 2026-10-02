use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use chrono::{Duration, Utc};
use rand::{Rng, distr::Alphanumeric};
use serde::{Deserialize, Serialize};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{env, net::SocketAddr, path::Path as FsPath, str::FromStr};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

const INDEX_HTML: &str = include_str!("../static/index.html");
const NOT_FOUND_HTML: &str = include_str!("../static/404.html");
const DEFAULT_MAX_BYTES: usize = 512 * 1024;

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
    max_bytes: usize,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
struct Paste {
    id: String,
    title: String,
    content: String,
    language: String,
    created_at: String,
    expires_at: Option<String>,
}

#[derive(Deserialize)]
struct CreatePaste {
    title: Option<String>,
    content: String,
    language: Option<String>,
    expiration: Option<String>,
}

#[derive(Serialize)]
struct CreatedPaste {
    id: String,
    url: String,
    raw_url: String,
    expires_at: Option<String>,
}

#[derive(Serialize)]
struct ApiError {
    error: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("snippaste=info,tower_http=info")),
        )
        .init();

    let database_url =
        env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://data/snippaste.db".into());
    if let Some(path) = database_url.strip_prefix("sqlite://") {
        if path != ":memory:" && !path.starts_with("/") {
            if let Some(parent) = FsPath::new(path).parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
    }

    let options = SqliteConnectOptions::from_str(&database_url)?.create_if_missing(true);
    let db = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS pastes (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            content TEXT NOT NULL,
            language TEXT NOT NULL,
            created_at TEXT NOT NULL,
            expires_at TEXT
        )",
    )
    .execute(&db)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_pastes_expiration ON pastes(expires_at)")
        .execute(&db)
        .await?;

    let max_bytes = env::var("MAX_PASTE_BYTES")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_BYTES);
    let state = AppState { db, max_bytes };

    let app = Router::new()
        .route("/", get(index))
        .route("/assets/ibm-plex-sans.woff2", get(ibm_plex_sans_font))
        .route("/assets/ibm-plex-mono.woff2", get(ibm_plex_mono_font))
        .route(
            "/assets/ibm-plex-mono-medium.woff2",
            get(ibm_plex_mono_medium_font),
        )
        .route("/api/pastes", post(create_paste))
        .route("/api/pastes/{id}", get(get_paste))
        .route("/api/pastes/{id}/raw", get(get_raw_paste))
        .route("/{id}/raw", get(get_raw_paste))
        .route("/{id}", get(view_paste))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(max_bytes + 16 * 1024))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let host = env::var("HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let address: SocketAddr = format!("{host}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!("SnipPaste listening at http://{address}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn not_found() -> (StatusCode, Html<&'static str>) {
    (StatusCode::NOT_FOUND, Html(NOT_FOUND_HTML))
}

async fn ibm_plex_sans_font() -> Response {
    font_response(include_bytes!("../static/fonts/ibm-plex-sans-latin.woff2"))
}

async fn ibm_plex_mono_font() -> Response {
    font_response(include_bytes!("../static/fonts/ibm-plex-mono-latin.woff2"))
}

async fn ibm_plex_mono_medium_font() -> Response {
    font_response(include_bytes!(
        "../static/fonts/ibm-plex-mono-medium-latin.woff2"
    ))
}

fn font_response(bytes: &'static [u8]) -> Response {
    let mut response = Response::new(axum::body::Body::from(bytes));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("font/woff2"));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    response
}

async fn create_paste(
    State(state): State<AppState>,
    Json(input): Json<CreatePaste>,
) -> Result<(StatusCode, Json<CreatedPaste>), (StatusCode, Json<ApiError>)> {
    if input.content.trim().is_empty() {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "A paste needs some content.",
        ));
    }
    if input.content.len() > state.max_bytes {
        return Err(error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "That paste is larger than the configured limit.",
        ));
    }

    let expiration = match input.expiration.as_deref().unwrap_or("never") {
        "never" => None,
        "1d" => Some(Utc::now() + Duration::days(1)),
        "7d" => Some(Utc::now() + Duration::days(7)),
        "30d" => Some(Utc::now() + Duration::days(30)),
        "1y" => Some(Utc::now() + Duration::days(365)),
        _ => {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "Choose a supported expiration period.",
            ));
        }
    };
    let expires_at = expiration.map(|date| date.to_rfc3339());
    let title = clean_label(input.title.unwrap_or_default(), 100, "Untitled paste");
    let language = clean_label(input.language.unwrap_or_default(), 40, "Plain text");
    let created_at = Utc::now().to_rfc3339();

    let id = loop {
        let candidate: String = rand::rng()
            .sample_iter(&Alphanumeric)
            .take(8)
            .map(char::from)
            .collect::<String>()
            .to_lowercase();
        let result = sqlx::query(
            "INSERT OR IGNORE INTO pastes (id, title, content, language, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&candidate)
        .bind(&title)
        .bind(&input.content)
        .bind(&language)
        .bind(&created_at)
        .bind(&expires_at)
        .execute(&state.db)
        .await
        .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "The paste could not be saved."))?;
        if result.rows_affected() == 1 {
            break candidate;
        }
    };

    let origin = env::var("PUBLIC_URL").unwrap_or_default();
    let url = if origin.is_empty() {
        format!("/{id}")
    } else {
        format!("{}{id}", origin.trim_end_matches('/').to_owned() + "/")
    };
    let raw_url = format!("{url}/raw");
    Ok((
        StatusCode::CREATED,
        Json(CreatedPaste {
            id,
            url,
            raw_url,
            expires_at,
        }),
    ))
}

async fn get_paste(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Paste>, (StatusCode, Json<ApiError>)> {
    let paste = find_paste(&state.db, &id).await?;
    Ok(Json(paste))
}

async fn get_raw_paste(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let paste = find_paste(&state.db, &id).await?;
    let mut response = paste.content.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

async fn view_paste(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let paste = match find_paste(&state.db, &id).await {
        Ok(paste) => paste,
        Err((StatusCode::NOT_FOUND, _)) => {
            return Ok((StatusCode::NOT_FOUND, Html(NOT_FOUND_HTML)).into_response());
        }
        Err(error) => return Err(error),
    };
    // Keep paste text from terminating the inline script tag when serialized into the page.
    let data = serde_json::to_string(&paste)
        .unwrap_or_else(|_| "{}".into())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    let page = INDEX_HTML.replace(
        "<!-- PASTE_DATA -->",
        &format!("<script>window.PASTE_DATA = {data};</script>"),
    );
    Ok(Html(page).into_response())
}

async fn find_paste(db: &SqlitePool, id: &str) -> Result<Paste, (StatusCode, Json<ApiError>)> {
    let paste = sqlx::query_as::<_, Paste>(
        "SELECT id, title, content, language, created_at, expires_at FROM pastes WHERE id = ? AND (expires_at IS NULL OR expires_at > ?)",
    )
    .bind(id)
    .bind(Utc::now().to_rfc3339())
    .fetch_optional(db)
    .await
    .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "The paste could not be loaded."))?;
    paste.ok_or_else(|| error(StatusCode::NOT_FOUND, "Paste not found or expired."))
}

fn clean_label(value: String, max_chars: usize, fallback: &str) -> String {
    let cleaned: String = value
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(max_chars)
        .collect();
    if cleaned.is_empty() {
        fallback.to_owned()
    } else {
        cleaned
    }
}

fn error(status: StatusCode, message: &str) -> (StatusCode, Json<ApiError>) {
    (
        status,
        Json(ApiError {
            error: message.to_owned(),
        }),
    )
}


use axum::{
    body::Body,
    extract::{FromRequest, Path as AxumPath, Query as AxumQuery, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use chrono::{NaiveDateTime, Utc};
use serde::{Deserialize, Serialize, Serializer};
use sha2::Digest as _;
use sqlx::mysql::{MySqlPool, MySqlPoolOptions};
use sqlx::Row;
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

const DEFAULT_CREDIT_LIMIT: i32 = 60000;


#[derive(Clone)]
struct AppState {
    pool: MySqlPool,
    sql_dir: PathBuf,
    db: Arc<DbConn>,
    http: reqwest::Client,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DbConn {
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
}

#[derive(Clone, Copy)]
struct AuthUser(Uuid);


#[tokio::main]
async fn main() {
    let dsn = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://isucon:isucon@127.0.0.1:3306/nrb2026".to_string());

    let db = parse_db_url(&dsn);

    let pool = MySqlPoolOptions::new()
        .max_connections(16)
        .connect(&dsn)
        .await
        .expect("connect to MySQL");

    let sql_dir = std::env::var("SQL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("sql"));

    let http = reqwest::Client::new();

    let state = AppState {
        pool,
        sql_dir,
        db: Arc::new(db),
        http,
    };

    let unauthed_api = Router::new()
        .route("/initialize", post(initialize))
        .route("/users", post(create_user))
        .route("/tags", get(list_tags));

    let authed_api = Router::new()
        .route("/me", get(get_me))
        .route("/campaigns", get(list_campaigns).post(create_campaign))
        .route("/campaigns/:id", get(get_campaign))
        .route("/campaigns/:id/image", get(get_campaign_image))
        .route("/campaigns/:id/join", post(join_campaign))
        .route("/saved_searches", post(create_saved_search))
        .route("/charges", get(list_charges))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    let api = unauthed_api
        .merge(authed_api)
        .fallback(|| async { StatusCode::NOT_FOUND });

    let static_dir: Option<PathBuf> = std::env::var_os("STATIC_DIR").map(PathBuf::from);

    let mut app = Router::<AppState>::new()
        .route("/healthz", get(healthz))
        .nest("/api", api);

    if let Some(dir) = static_dir {
        let index = dir.join("index.html");
        if !index.is_file() {
            panic!(
                "STATIC_DIR index.html not found: {} (set STATIC_DIR to the Vite build dir, or unset to skip SPA fallback)",
                index.display()
            );
        }
        let serve = ServeDir::new(&dir).not_found_service(ServeFile::new(index));
        app = app.fallback_service(serve);
    }

    let app = app.with_state(state);

    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()
        .expect("PORT must be a valid u16");
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("bind {addr}: {e}"));
    axum::serve(listener, app).await.unwrap();
}

fn parse_db_url(dsn: &str) -> DbConn {
    let u = url::Url::parse(dsn).expect("DATABASE_URL parse");
    DbConn {
        host: u.host_str().unwrap_or("127.0.0.1").to_string(),
        port: u.port().unwrap_or(3306),
        user: u.username().to_string(),
        password: u.password().unwrap_or("").to_string(),
        database: u.path().trim_start_matches('/').to_string(),
    }
}


#[derive(Debug)]
enum AppError {
    Unauthorized,
    BadRequest,
    PaymentRequired,
    NotFound,
    Conflict,
    PayloadTooLarge,
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match self {
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::BadRequest => StatusCode::BAD_REQUEST,
            AppError::PaymentRequired => StatusCode::PAYMENT_REQUIRED,
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::Conflict => StatusCode::CONFLICT,
            AppError::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            AppError::Internal(ref msg) => {
                eprintln!("internal error: {msg}");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, "").into_response()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        AppError::Internal(format!("sqlx: {e}"))
    }
}


struct JsonReq<T>(T);

#[axum::async_trait]
impl<T, S> FromRequest<S> for JsonReq<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, _state: &S) -> Result<Self, Self::Rejection> {
        let bytes = axum::body::to_bytes(req.into_body(), 1024 * 1024)
            .await
            .map_err(|_| AppError::BadRequest)?;
        let v: T = serde_json::from_slice(&bytes).map_err(|_| AppError::BadRequest)?;
        Ok(JsonReq(v))
    }
}


async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let header = req
        .headers()
        .get("x-user-id")
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthorized)?;
    let user_id = Uuid::parse_str(header).map_err(|_| AppError::Unauthorized)?;
    let exists: Option<(String,)> = sqlx::query_as("SELECT id FROM users WHERE id = ?")
        .bind(user_id.to_string())
        .fetch_optional(&state.pool)
        .await?;
    if exists.is_none() {
        return Err(AppError::Unauthorized);
    }
    req.extensions_mut().insert(AuthUser(user_id));
    Ok(next.run(req).await)
}


fn now_naive() -> NaiveDateTime {
    Utc::now().naive_utc()
}

fn fmt_dt(dt: NaiveDateTime) -> String {
    dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn serialize_dt<S: Serializer>(dt: &NaiveDateTime, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&fmt_dt(*dt))
}

fn validate_price(price: i32) -> Result<(), AppError> {
    if !(2000..=20000).contains(&price) {
        return Err(AppError::BadRequest);
    }
    Ok(())
}

fn validate_jpeg_image_b64(b64: &str) -> Result<Vec<u8>, AppError> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;

    let bytes = STANDARD.decode(b64).map_err(|_| AppError::BadRequest)?;
    if bytes.is_empty() {
        return Err(AppError::BadRequest);
    }
    if bytes.len() > 204_800 {
        return Err(AppError::PayloadTooLarge);
    }
    if bytes.len() < 3 || bytes[0] != 0xFF || bytes[1] != 0xD8 || bytes[2] != 0xFF {
        return Err(AppError::BadRequest);
    }
    Ok(bytes)
}

fn serialize_dt_opt<S: Serializer>(dt: &Option<NaiveDateTime>, s: S) -> Result<S::Ok, S::Error> {
    match dt {
        Some(dt) => s.serialize_str(&fmt_dt(*dt)),
        None => s.serialize_none(),
    }
}


#[derive(Serialize)]
struct CampaignRes {
    id: String,
    name: String,
    description: String,
    price: i32,
    goal_count: i32,
    current_count: i32,
    tags: Vec<String>,
    status: String,
    #[serde(serialize_with = "serialize_dt")]
    created_at: NaiveDateTime,
    #[serde(serialize_with = "serialize_dt_opt")]
    last_joined_at: Option<NaiveDateTime>,
    participants: Vec<ParticipantRes>,
}

#[derive(Serialize)]
struct ParticipantRes {
    user_id: String,
    name: String,
    #[serde(serialize_with = "serialize_dt")]
    joined_at: NaiveDateTime,
}


#[derive(Deserialize)]
struct InitReq {
    notification_webhook_url: String,
}

async fn initialize(
    State(state): State<AppState>,
    JsonReq(req): JsonReq<InitReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    run_mysql_file(&state, &state.sql_dir.join("schema.sql")).await?;
    let seed = state.sql_dir.join("seed.sql");
    let seed_path = if tokio::fs::try_exists(&seed).await.unwrap_or(false) {
        seed
    } else {
        state.sql_dir.join("seed.base.sql")
    };
    run_mysql_file(&state, &seed_path).await?;

    sqlx::query(
        "INSERT INTO app_config (name, value) VALUES (?, ?) \
         ON DUPLICATE KEY UPDATE value = VALUES(value)",
    )
    .bind("notification_webhook_url")
    .bind(&req.notification_webhook_url)
    .execute(&state.pool)
    .await?;

    Ok(Json(serde_json::json!({})))
}

async fn healthz(State(state): State<AppState>) -> StatusCode {
    match sqlx::query("SELECT 1").fetch_one(&state.pool).await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

async fn run_mysql_file(state: &AppState, path: &std::path::Path) -> Result<(), AppError> {
    let f = std::fs::File::open(path)
        .map_err(|e| AppError::Internal(format!("open {}: {e}", path.display())))?;
    let status = Command::new("mysql")
        .env("MYSQL_PWD", &state.db.password)
        .arg("-h")
        .arg(&state.db.host)
        .arg("-P")
        .arg(state.db.port.to_string())
        .arg("-u")
        .arg(&state.db.user)
        .arg("--protocol=TCP")
        .arg("--default-character-set=utf8mb4")
        .arg(&state.db.database)
        .stdin(Stdio::from(f))
        .status()
        .await
        .map_err(|e| AppError::Internal(format!("spawn mysql: {e}")))?;
    if !status.success() {
        return Err(AppError::Internal(format!("mysql exit {status}")));
    }
    Ok(())
}

#[derive(Deserialize)]
struct CreateUserReq {
    name: String,
}

#[derive(Serialize)]
struct UserRes {
    id: String,
    name: String,
    credit_limit: i32,
}

async fn create_user(
    State(state): State<AppState>,
    JsonReq(req): JsonReq<CreateUserReq>,
) -> Result<Json<UserRes>, AppError> {
    let len = req.name.chars().count();
    if len == 0 || len > 100 {
        return Err(AppError::BadRequest);
    }
    let id = Uuid::new_v4().to_string();
    let now = now_naive();
    let credit_limit = DEFAULT_CREDIT_LIMIT;
    sqlx::query("INSERT INTO users (id, name, credit_limit, created_at) VALUES (?, ?, ?, ?)")
        .bind(&id)
        .bind(&req.name)
        .bind(credit_limit)
        .bind(now)
        .execute(&state.pool)
        .await?;
    Ok(Json(UserRes {
        id,
        name: req.name,
        credit_limit,
    }))
}

#[derive(Serialize)]
struct MeRes {
    id: String,
    name: String,
    credit_limit: i32,
    credit_used: i32,
}

async fn get_me(
    State(state): State<AppState>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
) -> Result<Json<MeRes>, AppError> {
    let row: Option<(String, i32)> =
        sqlx::query_as("SELECT name, credit_limit FROM users WHERE id = ?")
            .bind(user_id.to_string())
            .fetch_optional(&state.pool)
            .await?;
    let (name, credit_limit) = row.ok_or(AppError::Unauthorized)?;

    let part_rows: Vec<(String,)> = sqlx::query_as(
        "SELECT campaign_id FROM campaign_participants WHERE user_id = ?",
    )
    .bind(user_id.to_string())
    .fetch_all(&state.pool)
    .await?;
    let mut credit_used: i64 = 0;
    for (cid,) in part_rows {
        let row = sqlx::query("SELECT price, goal_count FROM campaigns WHERE id = ?")
            .bind(&cid)
            .fetch_one(&state.pool)
            .await?;
        let price: i32 = row.try_get("price")?;
        let goal_count: i32 = row.try_get("goal_count")?;
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM campaign_participants WHERE campaign_id = ?",
        )
        .bind(&cid)
        .fetch_one(&state.pool)
        .await?;
        if (count as i32) < goal_count {
            credit_used += price as i64;
        }
    }

    Ok(Json(MeRes {
        id: user_id.to_string(),
        name,
        credit_limit,
        credit_used: credit_used as i32,
    }))
}

async fn list_tags(State(state): State<AppState>) -> Result<Json<Vec<String>>, AppError> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT name FROM tags")
        .fetch_all(&state.pool)
        .await?;
    Ok(Json(rows.into_iter().map(|(n,)| n).collect()))
}

#[derive(Deserialize)]
struct ListCampaignsQuery {
    tags: Option<String>,
    sort: Option<String>,
}

async fn list_campaigns(
    State(state): State<AppState>,
    AxumQuery(q): AxumQuery<ListCampaignsQuery>,
) -> Result<Json<Vec<CampaignRes>>, AppError> {
    let tag_filter: Vec<String> = match q.tags.as_deref() {
        Some(s) if !s.is_empty() => {
            let parts: Vec<String> = s.split(',').map(|p| p.to_string()).collect();
            if parts.len() > 3 {
                return Err(AppError::BadRequest);
            }
            let mut seen = HashSet::new();
            for p in &parts {
                if !seen.insert(p.clone()) {
                    return Err(AppError::BadRequest);
                }
            }
            for p in &parts {
                let r: Option<(String,)> = sqlx::query_as("SELECT id FROM tags WHERE name = ?")
                    .bind(p)
                    .fetch_optional(&state.pool)
                    .await?;
                if r.is_none() {
                    return Err(AppError::BadRequest);
                }
            }
            parts
        }
        _ => Vec::new(),
    };

    let sort_mode = match q.sort.as_deref() {
        Some("active") => "active",
        Some("new") | None => "new",
        _ => return Err(AppError::BadRequest),
    };

    let id_rows: Vec<(String,)> = sqlx::query_as("SELECT id FROM campaigns")
        .fetch_all(&state.pool)
        .await?;
    let mut conn = state.pool.acquire().await?;
    let mut all: Vec<CampaignRes> = Vec::new();
    for (cid,) in id_rows {
        if let Some(c) = hydrate_campaign(&mut conn, &cid).await? {
            all.push(c);
        }
    }
    drop(conn);

    let want: HashSet<&str> = tag_filter.iter().map(|s| s.as_str()).collect();
    all.retain(|c| {
        if c.status != "open" {
            return false;
        }
        if !want.is_empty() {
            let have: HashSet<&str> = c.tags.iter().map(|s| s.as_str()).collect();
            for w in &want {
                if !have.contains(w) {
                    return false;
                }
            }
        }
        true
    });

    if sort_mode == "active" {
        all.sort_by(|a, b| {
            let ak = a.last_joined_at.unwrap_or(a.created_at);
            let bk = b.last_joined_at.unwrap_or(b.created_at);
            bk.cmp(&ak)
        });
    } else {
        all.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    }

    all.truncate(30);
    Ok(Json(all))
}

#[derive(Deserialize)]
struct CreateCampaignReq {
    name: String,
    description: String,
    price: i32,
    goal_count: i32,
    tags: Vec<String>,
    image: String,
}

async fn create_campaign(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    JsonReq(req): JsonReq<CreateCampaignReq>,
) -> Result<(StatusCode, Json<CampaignRes>), AppError> {
    let name_len = req.name.chars().count();
    if name_len == 0 || name_len > 100 {
        return Err(AppError::BadRequest);
    }
    let desc_len = req.description.chars().count();
    if desc_len == 0 || desc_len > 1000 {
        return Err(AppError::BadRequest);
    }
    validate_price(req.price)?;
    if req.goal_count < 2 || req.goal_count > 20 {
        return Err(AppError::BadRequest);
    }
    if req.tags.len() > 10 {
        return Err(AppError::BadRequest);
    }
    let mut seen_names = HashSet::new();
    for t in &req.tags {
        if !seen_names.insert(t.clone()) {
            return Err(AppError::BadRequest);
        }
    }
    let image_bytes = validate_jpeg_image_b64(&req.image)?;
    let mut tag_ids: Vec<String> = Vec::new();
    for t in &req.tags {
        let r: Option<(String,)> = sqlx::query_as("SELECT id FROM tags WHERE name = ?")
            .bind(t)
            .fetch_optional(&state.pool)
            .await?;
        match r {
            Some((tid,)) => {
                if tag_ids.iter().any(|id| id == &tid) {
                    return Err(AppError::BadRequest);
                }
                tag_ids.push(tid);
            }
            None => return Err(AppError::BadRequest),
        }
    }

    let id = Uuid::new_v4().to_string();
    let now = now_naive();
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO campaigns (id, name, description, price, goal_count, image, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&req.name)
    .bind(&req.description)
    .bind(req.price)
    .bind(req.goal_count)
    .bind(&image_bytes)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    for tid in &tag_ids {
        sqlx::query("INSERT INTO campaign_tags (campaign_id, tag_id, created_at) VALUES (?, ?, ?)")
            .bind(&id)
            .bind(tid)
            .bind(now)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    let res = hydrate_campaign_via_pool(&state.pool, &id)
        .await?
        .ok_or(AppError::Internal("created campaign vanished".into()))?;
    Ok((StatusCode::CREATED, Json(res)))
}

async fn get_campaign_image(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, AppError> {
    let row: Option<(Vec<u8>,)> =
        sqlx::query_as("SELECT image FROM campaigns WHERE id = ?")
            .bind(&id)
            .fetch_optional(&state.pool)
            .await?;
    let bytes = match row {
        Some((b,)) => b,
        None => return Err(AppError::NotFound),
    };
    let hash_hex = hex::encode(sha2::Sha256::digest(&bytes));
    let etag = format!("\"{hash_hex}\"");
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::ETAG, etag.as_str()),
        ],
        Body::from(bytes),
    )
        .into_response())
}

async fn get_campaign(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<CampaignRes>, AppError> {
    match hydrate_campaign_via_pool(&state.pool, &id).await? {
        Some(c) => Ok(Json(c)),
        None => Err(AppError::NotFound),
    }
}

#[derive(Deserialize)]
struct JoinReq {}

async fn join_campaign(
    State(state): State<AppState>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    AxumPath(campaign_id): AxumPath<String>,
    JsonReq(_): JsonReq<JoinReq>,
) -> Result<Json<CampaignRes>, AppError> {
    let mut tx = state.pool.begin().await?;

    let (credit_limit,): (i32,) = sqlx::query_as(
        "SELECT credit_limit FROM users WHERE id = ? FOR UPDATE",
    )
    .bind(user_id.to_string())
    .fetch_one(&mut *tx)
    .await?;

    let row = sqlx::query("SELECT goal_count, price FROM campaigns WHERE id = ? FOR UPDATE")
        .bind(&campaign_id)
        .fetch_optional(&mut *tx)
        .await?;
    let (goal_count, price): (i32, i32) = match row {
        Some(r) => (r.try_get("goal_count")?, r.try_get("price")?),
        None => return Err(AppError::NotFound),
    };

    let (before,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM campaign_participants WHERE campaign_id = ?")
            .bind(&campaign_id)
            .fetch_one(&mut *tx)
            .await?;
    let before = before as i32;

    if before >= goal_count {
        return Err(AppError::Conflict);
    }

    let dup: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM campaign_participants WHERE campaign_id = ? AND user_id = ?",
    )
    .bind(&campaign_id)
    .bind(user_id.to_string())
    .fetch_optional(&mut *tx)
    .await?;
    if dup.is_some() {
        return Err(AppError::Conflict);
    }

    let part_rows: Vec<(String,)> = sqlx::query_as(
        "SELECT campaign_id FROM campaign_participants WHERE user_id = ?",
    )
    .bind(user_id.to_string())
    .fetch_all(&mut *tx)
    .await?;
    let mut before_credit_used: i64 = 0;
    for (cid,) in part_rows {
        let row = sqlx::query("SELECT price, goal_count FROM campaigns WHERE id = ?")
            .bind(&cid)
            .fetch_one(&mut *tx)
            .await?;
        let p: i32 = row.try_get("price")?;
        let g: i32 = row.try_get("goal_count")?;
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM campaign_participants WHERE campaign_id = ?",
        )
        .bind(&cid)
        .fetch_one(&mut *tx)
        .await?;
        if (count as i32) < g {
            before_credit_used += p as i64;
        }
    }
    if before_credit_used as i32 + price > credit_limit {
        return Err(AppError::PaymentRequired);
    }

    let participant_id = Uuid::new_v4().to_string();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO campaign_participants (id, campaign_id, user_id, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(&participant_id)
    .bind(&campaign_id)
    .bind(user_id.to_string())
    .bind(now)
    .execute(&mut *tx)
    .await?;
    let after = before + 1;

    let mut webhook_user_ids: Vec<String> = Vec::new();
    if after == goal_count - 1 {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT ss.user_id \
             FROM saved_searches ss \
             JOIN saved_search_tags sst ON sst.saved_search_id = ss.id \
             LEFT JOIN campaign_tags ct ON ct.campaign_id = ? AND ct.tag_id = sst.tag_id \
             GROUP BY ss.id, ss.user_id \
             HAVING COUNT(*) = COUNT(ct.tag_id)",
        )
        .bind(&campaign_id)
        .fetch_all(&mut *tx)
        .await?;
        webhook_user_ids = rows.into_iter().map(|(u,)| u).collect();
    }

    if after == goal_count {
        let part_rows: Vec<(String,)> =
            sqlx::query_as("SELECT id FROM campaign_participants WHERE campaign_id = ?")
                .bind(&campaign_id)
                .fetch_all(&mut *tx)
                .await?;
        for (pid,) in part_rows {
            sqlx::query(
                "INSERT INTO charges (id, campaign_participant_id, created_at) VALUES (?, ?, ?)",
            )
            .bind(Uuid::new_v4().to_string())
            .bind(pid)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
    }

    let response_campaign = hydrate_campaign(&mut *tx, &campaign_id)
        .await?
        .ok_or(AppError::Internal("modified campaign vanished".into()))?;

    let webhook_url = if webhook_user_ids.is_empty() {
        String::new()
    } else {
        let url_row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM app_config WHERE name = 'notification_webhook_url'")
                .fetch_optional(&mut *tx)
                .await?;
        url_row.map(|(v,)| v).unwrap_or_default()
    };

    tx.commit().await?;

    if !webhook_user_ids.is_empty() && !webhook_url.is_empty() {
        for uid in &webhook_user_ids {
            let body = serde_json::json!({
                "type": "campaign_closing_soon",
                "user_id": uid,
                "campaign": {
                    "id": &response_campaign.id,
                    "name": &response_campaign.name,
                    "description": &response_campaign.description,
                    "price": response_campaign.price,
                    "goal_count": response_campaign.goal_count,
                    "current_count": response_campaign.current_count,
                    "tags": &response_campaign.tags,
                    "status": &response_campaign.status,
                    "created_at": fmt_dt(response_campaign.created_at),
                    "last_joined_at": response_campaign.last_joined_at.map(fmt_dt),
                }
            });
            if let Err(e) = state.http.post(&webhook_url).json(&body).send().await {
                eprintln!("webhook send to {webhook_url} for user {uid}: {e}");
            }
        }
    }

    Ok(Json(response_campaign))
}

#[derive(Deserialize)]
struct CreateSavedSearchReq {
    tags: Vec<String>,
}

async fn create_saved_search(
    State(state): State<AppState>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    JsonReq(req): JsonReq<CreateSavedSearchReq>,
) -> Result<StatusCode, AppError> {
    if req.tags.is_empty() || req.tags.len() > 3 {
        return Err(AppError::BadRequest);
    }
    let mut seen_names = HashSet::new();
    for t in &req.tags {
        if !seen_names.insert(t.clone()) {
            return Err(AppError::BadRequest);
        }
    }
    let mut tag_ids: Vec<String> = Vec::new();
    for t in &req.tags {
        let r: Option<(String,)> = sqlx::query_as("SELECT id FROM tags WHERE name = ?")
            .bind(t)
            .fetch_optional(&state.pool)
            .await?;
        match r {
            Some((tid,)) => {
                if tag_ids.iter().any(|id| id == &tid) {
                    return Err(AppError::BadRequest);
                }
                tag_ids.push(tid);
            }
            None => return Err(AppError::BadRequest),
        }
    }

    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT id FROM users WHERE id = ? FOR UPDATE")
        .bind(user_id.to_string())
        .fetch_one(&mut *tx)
        .await?;
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM saved_searches WHERE user_id = ?")
        .bind(user_id.to_string())
        .fetch_one(&mut *tx)
        .await?;
    if count >= 10 {
        return Err(AppError::Conflict);
    }

    let ss_id = Uuid::new_v4().to_string();
    let now = now_naive();
    sqlx::query("INSERT INTO saved_searches (id, user_id, created_at) VALUES (?, ?, ?)")
        .bind(&ss_id)
        .bind(user_id.to_string())
        .bind(now)
        .execute(&mut *tx)
        .await?;
    for tid in &tag_ids {
        sqlx::query(
            "INSERT INTO saved_search_tags (saved_search_id, tag_id, created_at) VALUES (?, ?, ?)",
        )
        .bind(&ss_id)
        .bind(tid)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::CREATED)
}

#[derive(Serialize)]
struct ChargeRes {
    id: String,
    amount: i32,
    campaign: ChargeCampaign,
    #[serde(serialize_with = "serialize_dt")]
    created_at: NaiveDateTime,
}

#[derive(Serialize)]
struct ChargeCampaign {
    id: String,
    name: String,
    price: i32,
}

async fn list_charges(
    State(state): State<AppState>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
) -> Result<Json<Vec<ChargeRes>>, AppError> {
    let rows: Vec<(String, NaiveDateTime, String, String, i32)> = sqlx::query_as(
        "SELECT ch.id, ch.created_at, c.id, c.name, c.price \
         FROM charges ch \
         JOIN campaign_participants cp ON ch.campaign_participant_id = cp.id \
         JOIN campaigns c ON cp.campaign_id = c.id \
         WHERE cp.user_id = ? \
         ORDER BY ch.created_at DESC",
    )
    .bind(user_id.to_string())
    .fetch_all(&state.pool)
    .await?;
    let res: Vec<ChargeRes> = rows
        .into_iter()
        .map(|(id, ca, cid, name, price)| ChargeRes {
            id,
            amount: price,
            campaign: ChargeCampaign {
                id: cid,
                name,
                price,
            },
            created_at: ca,
        })
        .collect();
    Ok(Json(res))
}


async fn hydrate_campaign(
    conn: &mut sqlx::MySqlConnection,
    id: &str,
) -> Result<Option<CampaignRes>, AppError> {
    let row = sqlx::query(
        "SELECT id, name, description, price, goal_count, created_at \
         FROM campaigns WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    let row = match row {
        Some(r) => r,
        None => return Ok(None),
    };

    let id: String = row.try_get("id")?;
    let name: String = row.try_get("name")?;
    let description: String = row.try_get("description")?;
    let price: i32 = row.try_get("price")?;
    let goal_count: i32 = row.try_get("goal_count")?;
    let created_at: NaiveDateTime = row.try_get("created_at")?;

    let tag_rows: Vec<(String,)> = sqlx::query_as(
        "SELECT t.name FROM campaign_tags ct JOIN tags t ON ct.tag_id = t.id WHERE ct.campaign_id = ?",
    )
    .bind(&id)
    .fetch_all(&mut *conn)
    .await?;
    let tags: Vec<String> = tag_rows.into_iter().map(|(n,)| n).collect();

    let part_rows: Vec<(String, String, NaiveDateTime)> = sqlx::query_as(
        "SELECT cp.user_id, u.name, cp.created_at \
         FROM campaign_participants cp JOIN users u ON cp.user_id = u.id \
         WHERE cp.campaign_id = ? ORDER BY cp.created_at ASC",
    )
    .bind(&id)
    .fetch_all(&mut *conn)
    .await?;
    let participants: Vec<ParticipantRes> = part_rows
        .into_iter()
        .map(|(uid, n, t)| ParticipantRes {
            user_id: uid,
            name: n,
            joined_at: t,
        })
        .collect();

    let current_count = participants.len() as i32;
    let last_joined_at = participants.last().map(|p| p.joined_at);
    let status = if current_count >= goal_count {
        "closed"
    } else {
        "open"
    };

    Ok(Some(CampaignRes {
        id,
        name,
        description,
        price,
        goal_count,
        current_count,
        tags,
        status: status.to_string(),
        created_at,
        last_joined_at,
        participants,
    }))
}

async fn hydrate_campaign_via_pool(
    pool: &MySqlPool,
    id: &str,
) -> Result<Option<CampaignRes>, AppError> {
    let mut conn = pool.acquire().await?;
    hydrate_campaign(&mut conn, id).await
}


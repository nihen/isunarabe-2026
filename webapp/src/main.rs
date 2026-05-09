use axum::{
    body::Body,
    extract::{FromRequest, Path as AxumPath, Query as AxumQuery, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use chrono::{NaiveDateTime, Utc};
use serde::{Deserialize, Serialize, Serializer};
use sha2::Digest as _;
use sqlx::mysql::{MySql, MySqlPool, MySqlPoolOptions};
use sqlx::QueryBuilder;
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::{mpsc, RwLock};
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

const DEFAULT_CREDIT_LIMIT: i32 = 60000;

#[derive(Clone)]
struct AppState {
    pool: MySqlPool,
    sql_dir: PathBuf,
    db: Arc<DbConn>,
    cache: Arc<AppCache>,
    webhook_tx: mpsc::Sender<WebhookMessage>,
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

#[derive(Clone)]
struct WebhookMessage {
    url: String,
    body: serde_json::Value,
}

#[derive(Default)]
struct AppCache {
    user_ids: RwLock<HashSet<String>>,
    tag_ids_by_name: RwLock<HashMap<String, String>>,
    tags_json: RwLock<Option<Arc<Vec<u8>>>>,
    campaign_json: RwLock<HashMap<String, Arc<Vec<u8>>>>,
    campaign_image: RwLock<HashMap<String, ImageCache>>,
    list_campaigns_json: RwLock<HashMap<String, Arc<Vec<u8>>>>,
    me_json: RwLock<HashMap<String, Arc<Vec<u8>>>>,
    charges_json: RwLock<HashMap<String, Arc<Vec<u8>>>>,
}

#[derive(Clone)]
struct ImageCache {
    bytes: Arc<Vec<u8>>,
    etag: String,
}

impl AppCache {
    async fn clear_all(&self) {
        self.user_ids.write().await.clear();
        self.tag_ids_by_name.write().await.clear();
        *self.tags_json.write().await = None;
        self.campaign_json.write().await.clear();
        self.campaign_image.write().await.clear();
        self.list_campaigns_json.write().await.clear();
        self.me_json.write().await.clear();
        self.charges_json.write().await.clear();
    }

    async fn clear_list_campaigns(&self) {
        self.list_campaigns_json.write().await.clear();
    }

    async fn clear_users<'a, I>(&self, user_ids: I)
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut me_json = self.me_json.write().await;
        let mut charges_json = self.charges_json.write().await;
        for user_id in user_ids {
            me_json.remove(user_id);
            charges_json.remove(user_id);
        }
    }

    async fn insert_user(&self, user_id: String) {
        self.user_ids.write().await.insert(user_id);
    }
}

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
    let (webhook_tx, webhook_rx) = mpsc::channel(8192);
    tokio::spawn(webhook_worker(http.clone(), webhook_rx));

    let state = AppState {
        pool,
        sql_dir,
        db: Arc::new(db),
        cache: Arc::new(AppCache::default()),
        webhook_tx,
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

async fn webhook_worker(http: reqwest::Client, mut rx: mpsc::Receiver<WebhookMessage>) {
    while let Some(message) = rx.recv().await {
        if let Err(e) = http.post(&message.url).json(&message.body).send().await {
            eprintln!("webhook send to {}: {e}", message.url);
        }
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

fn response_from_json_bytes(body: Arc<Vec<u8>>) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        Body::from(body.as_ref().clone()),
    )
        .into_response()
}

fn serialize_json<T: Serialize>(value: &T) -> Result<Arc<Vec<u8>>, AppError> {
    let bytes = serde_json::to_vec(value).map_err(|e| AppError::Internal(format!("json: {e}")))?;
    Ok(Arc::new(bytes))
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
    let user_id_s = user_id.to_string();
    if state.cache.user_ids.read().await.contains(&user_id_s) {
        req.extensions_mut().insert(AuthUser(user_id));
        return Ok(next.run(req).await);
    }

    let exists: Option<(String,)> = sqlx::query_as("SELECT id FROM users WHERE id = ?")
        .bind(&user_id_s)
        .fetch_optional(&state.pool)
        .await?;
    if exists.is_none() {
        return Err(AppError::Unauthorized);
    }
    state.cache.insert_user(user_id_s).await;
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

#[derive(Clone, Serialize)]
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

#[derive(Clone, Serialize)]
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

    state.cache.clear_all().await;
    warm_read_cache(&state).await?;

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
    state.cache.insert_user(id.clone()).await;
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
) -> Result<Response, AppError> {
    let user_id_s = user_id.to_string();
    if let Some(body) = state.cache.me_json.read().await.get(&user_id_s).cloned() {
        return Ok(response_from_json_bytes(body));
    }

    let row: Option<(String, i32)> =
        sqlx::query_as("SELECT name, credit_limit FROM users WHERE id = ?")
            .bind(&user_id_s)
            .fetch_optional(&state.pool)
            .await?;
    let (name, credit_limit) = row.ok_or(AppError::Unauthorized)?;

    let credit_used = fetch_open_credit_used(&state.pool, &user_id_s).await?;

    let res = MeRes {
        id: user_id_s.clone(),
        name,
        credit_limit,
        credit_used: credit_used as i32,
    };
    let body = serialize_json(&res)?;
    state.cache.me_json.write().await.insert(user_id_s, body.clone());
    Ok(response_from_json_bytes(body))
}

async fn fetch_open_credit_used(pool: &MySqlPool, user_id: &str) -> Result<i64, AppError> {
    let (credit_used,): (i64,) = sqlx::query_as(
        "SELECT CAST(COALESCE(SUM(c.price), 0) AS SIGNED) \
         FROM campaign_participants cp \
         JOIN campaigns c ON c.id = cp.campaign_id \
         JOIN ( \
             SELECT campaign_id, COUNT(*) AS current_count \
             FROM campaign_participants \
             GROUP BY campaign_id \
         ) cc ON cc.campaign_id = c.id \
         WHERE cp.user_id = ? AND cc.current_count < c.goal_count",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(credit_used)
}

async fn resolve_tag_ids(state: &AppState, tag_names: &[String]) -> Result<Vec<String>, AppError> {
    let cached = state.cache.tag_ids_by_name.read().await;
    if !cached.is_empty() {
        let mut tag_ids = Vec::with_capacity(tag_names.len());
        for name in tag_names {
            let tag_id = cached.get(name).ok_or(AppError::BadRequest)?;
            tag_ids.push(tag_id.clone());
        }
        return Ok(tag_ids);
    }
    drop(cached);

    let rows: Vec<(String, String)> = sqlx::query_as("SELECT name, id FROM tags")
        .fetch_all(&state.pool)
        .await?;
    let tag_ids_by_name: HashMap<String, String> = rows.into_iter().collect();
    let mut tag_ids = Vec::with_capacity(tag_names.len());
    for name in tag_names {
        let tag_id = tag_ids_by_name.get(name).ok_or(AppError::BadRequest)?;
        tag_ids.push(tag_id.clone());
    }
    *state.cache.tag_ids_by_name.write().await = tag_ids_by_name.clone();
    Ok(tag_ids)
}

async fn list_tags(State(state): State<AppState>) -> Result<Response, AppError> {
    if let Some(body) = state.cache.tags_json.read().await.clone() {
        return Ok(response_from_json_bytes(body));
    }

    let rows: Vec<(String, String)> = sqlx::query_as("SELECT name, id FROM tags")
        .fetch_all(&state.pool)
        .await?;
    let mut tags = Vec::with_capacity(rows.len());
    let mut tag_ids_by_name = HashMap::with_capacity(rows.len());
    for (name, id) in rows {
        tags.push(name.clone());
        tag_ids_by_name.insert(name, id);
    }
    let body = serialize_json(&tags)?;
    *state.cache.tags_json.write().await = Some(body.clone());
    *state.cache.tag_ids_by_name.write().await = tag_ids_by_name.clone();
    Ok(response_from_json_bytes(body))
}

#[derive(Deserialize)]
struct ListCampaignsQuery {
    tags: Option<String>,
    sort: Option<String>,
}

async fn list_campaigns(
    State(state): State<AppState>,
    AxumQuery(q): AxumQuery<ListCampaignsQuery>,
) -> Result<Response, AppError> {
    let mut tag_ids: Vec<String> = match q.tags.as_deref() {
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
            resolve_tag_ids(&state, &parts).await?
        }
        _ => Vec::new(),
    };

    let sort_mode = match q.sort.as_deref() {
        Some("active") => "active",
        Some("new") | None => "new",
        _ => return Err(AppError::BadRequest),
    };
    tag_ids.sort();
    let list_cache_key = format!("sort={sort_mode};tags={}", tag_ids.join(","));
    if let Some(body) = state.cache.list_campaigns_json.read().await.get(&list_cache_key).cloned() {
        return Ok(response_from_json_bytes(body));
    }

    let mut qb = QueryBuilder::<MySql>::new(
        "SELECT c.id, c.name, c.description, c.price, c.goal_count, c.created_at, \
         COUNT(cp.id) AS current_count, MAX(cp.created_at) AS last_joined_at \
         FROM campaigns c \
         LEFT JOIN campaign_participants cp ON cp.campaign_id = c.id",
    );
    if !tag_ids.is_empty() {
        qb.push(" WHERE c.id IN (SELECT ct.campaign_id FROM campaign_tags ct WHERE ct.tag_id IN (");
        let mut separated = qb.separated(", ");
        for tag_id in &tag_ids {
            separated.push_bind(tag_id);
        }
        drop(separated);
        qb.push(") GROUP BY ct.campaign_id HAVING COUNT(DISTINCT ct.tag_id) = ");
        qb.push_bind(tag_ids.len() as i64);
        qb.push(")");
    }
    qb.push(
        " GROUP BY c.id, c.name, c.description, c.price, c.goal_count, c.created_at \
         HAVING COUNT(cp.id) < c.goal_count ORDER BY ",
    );
    if sort_mode == "active" {
        qb.push("COALESCE(MAX(cp.created_at), c.created_at) DESC");
    } else {
        qb.push("c.created_at DESC");
    }
    qb.push(" LIMIT 30");

    let rows: Vec<(
        String,
        String,
        String,
        i32,
        i32,
        NaiveDateTime,
        i64,
        Option<NaiveDateTime>,
    )> = qb.build_query_as().fetch_all(&state.pool).await?;
    let campaign_ids: Vec<String> = rows.iter().map(|row| row.0.clone()).collect();
    let tags_by_campaign = fetch_tags_by_campaign(&state.pool, &campaign_ids).await?;
    let participants_by_campaign =
        fetch_participants_by_campaign(&state.pool, &campaign_ids).await?;

    let all: Vec<CampaignRes> = rows
        .into_iter()
        .map(
            |(
                id,
                name,
                description,
                price,
                goal_count,
                created_at,
                _current_count,
                last_joined_at,
            )| {
                let tags = tags_by_campaign.get(&id).cloned().unwrap_or_default();
                let participants = participants_by_campaign
                    .get(&id)
                    .cloned()
                    .unwrap_or_default();
                let current_count = participants.len() as i32;
                CampaignRes {
                    id,
                    name,
                    description,
                    price,
                    goal_count,
                    current_count,
                    tags,
                    status: if current_count >= goal_count {
                        "closed"
                    } else {
                        "open"
                    }
                    .to_string(),
                    created_at,
                    last_joined_at,
                    participants,
                }
            },
        )
        .collect();

    let body = serialize_json(&all)?;
    state.cache.list_campaigns_json.write().await.insert(list_cache_key, body.clone());
    Ok(response_from_json_bytes(body))
}

async fn fetch_tags_by_campaign(
    pool: &MySqlPool,
    campaign_ids: &[String],
) -> Result<HashMap<String, Vec<String>>, AppError> {
    let mut tags_by_campaign: HashMap<String, Vec<String>> = HashMap::new();
    if campaign_ids.is_empty() {
        return Ok(tags_by_campaign);
    }

    let mut qb = QueryBuilder::<MySql>::new(
        "SELECT ct.campaign_id, t.name \
         FROM campaign_tags ct \
         JOIN tags t ON ct.tag_id = t.id \
         WHERE ct.campaign_id IN (",
    );
    let mut separated = qb.separated(", ");
    for campaign_id in campaign_ids {
        separated.push_bind(campaign_id);
    }
    drop(separated);
    qb.push(")");

    let rows: Vec<(String, String)> = qb.build_query_as().fetch_all(pool).await?;
    for (campaign_id, tag_name) in rows {
        tags_by_campaign
            .entry(campaign_id)
            .or_default()
            .push(tag_name);
    }
    Ok(tags_by_campaign)
}

async fn fetch_participants_by_campaign(
    pool: &MySqlPool,
    campaign_ids: &[String],
) -> Result<HashMap<String, Vec<ParticipantRes>>, AppError> {
    let mut participants_by_campaign: HashMap<String, Vec<ParticipantRes>> = HashMap::new();
    if campaign_ids.is_empty() {
        return Ok(participants_by_campaign);
    }

    let mut qb = QueryBuilder::<MySql>::new(
        "SELECT cp.campaign_id, cp.user_id, u.name, cp.created_at \
         FROM campaign_participants cp \
         JOIN users u ON cp.user_id = u.id \
         WHERE cp.campaign_id IN (",
    );
    let mut separated = qb.separated(", ");
    for campaign_id in campaign_ids {
        separated.push_bind(campaign_id);
    }
    drop(separated);
    qb.push(") ORDER BY cp.campaign_id, cp.created_at ASC");

    let rows: Vec<(String, String, String, NaiveDateTime)> =
        qb.build_query_as().fetch_all(pool).await?;
    for (campaign_id, user_id, name, joined_at) in rows {
        participants_by_campaign
            .entry(campaign_id)
            .or_default()
            .push(ParticipantRes {
                user_id,
                name,
                joined_at,
            });
    }
    Ok(participants_by_campaign)
}

async fn warm_read_cache(state: &AppState) -> Result<(), AppError> {
    let tag_rows: Vec<(String, String)> = sqlx::query_as("SELECT name, id FROM tags")
        .fetch_all(&state.pool)
        .await?;
    let mut tags = Vec::with_capacity(tag_rows.len());
    let mut tag_ids_by_name = HashMap::with_capacity(tag_rows.len());
    for (name, id) in tag_rows {
        tags.push(name.clone());
        tag_ids_by_name.insert(name, id);
    }
    *state.cache.tags_json.write().await = Some(serialize_json(&tags)?);
    *state.cache.tag_ids_by_name.write().await = tag_ids_by_name.clone();

    let user_rows: Vec<(String, String, i32)> =
        sqlx::query_as("SELECT id, name, credit_limit FROM users")
            .fetch_all(&state.pool)
            .await?;
    *state.cache.user_ids.write().await = user_rows
        .iter()
        .map(|(id, _, _)| id.clone())
        .collect::<HashSet<_>>();

    let credit_rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT cp.user_id, CAST(COALESCE(SUM(c.price), 0) AS SIGNED) \
         FROM campaign_participants cp \
         JOIN campaigns c ON c.id = cp.campaign_id \
         JOIN ( \
             SELECT campaign_id, COUNT(*) AS current_count \
             FROM campaign_participants \
             GROUP BY campaign_id \
         ) cc ON cc.campaign_id = c.id \
         WHERE cc.current_count < c.goal_count \
         GROUP BY cp.user_id",
    )
    .fetch_all(&state.pool)
    .await?;
    let credit_by_user: HashMap<String, i64> = credit_rows.into_iter().collect();
    let mut me_json = HashMap::with_capacity(user_rows.len());
    for (id, name, credit_limit) in &user_rows {
        let credit_used = credit_by_user.get(id).copied().unwrap_or(0) as i32;
        let me = MeRes {
            id: id.clone(),
            name: name.clone(),
            credit_limit: *credit_limit,
            credit_used,
        };
        me_json.insert(id.clone(), serialize_json(&me)?);
    }
    *state.cache.me_json.write().await = me_json;

    let charge_rows: Vec<(String, String, NaiveDateTime, String, String, i32)> = sqlx::query_as(
        "SELECT cp.user_id, ch.id, ch.created_at, c.id, c.name, c.price \
         FROM charges ch \
         JOIN campaign_participants cp ON ch.campaign_participant_id = cp.id \
         JOIN campaigns c ON cp.campaign_id = c.id \
         ORDER BY ch.created_at DESC",
    )
    .fetch_all(&state.pool)
    .await?;
    let mut charges_by_user: HashMap<String, Vec<ChargeRes>> = user_rows
        .iter()
        .map(|(id, _, _)| (id.clone(), Vec::new()))
        .collect();
    for (user_id, id, created_at, campaign_id, name, price) in charge_rows {
        charges_by_user.entry(user_id).or_default().push(ChargeRes {
            id,
            amount: price,
            campaign: ChargeCampaign {
                id: campaign_id,
                name,
                price,
            },
            created_at,
        });
    }
    let mut charges_json = HashMap::with_capacity(charges_by_user.len());
    for (user_id, charges) in charges_by_user {
        charges_json.insert(user_id, serialize_json(&charges)?);
    }
    *state.cache.charges_json.write().await = charges_json;

    let rows: Vec<(
        String,
        String,
        String,
        i32,
        i32,
        NaiveDateTime,
        i64,
        Option<NaiveDateTime>,
    )> = sqlx::query_as(
        "SELECT c.id, c.name, c.description, c.price, c.goal_count, c.created_at, \
         COUNT(cp.id) AS current_count, MAX(cp.created_at) AS last_joined_at \
         FROM campaigns c \
         LEFT JOIN campaign_participants cp ON cp.campaign_id = c.id \
         GROUP BY c.id, c.name, c.description, c.price, c.goal_count, c.created_at",
    )
    .fetch_all(&state.pool)
    .await?;
    let campaign_ids: Vec<String> = rows.iter().map(|row| row.0.clone()).collect();
    let tags_by_campaign = fetch_tags_by_campaign(&state.pool, &campaign_ids).await?;
    let participants_by_campaign =
        fetch_participants_by_campaign(&state.pool, &campaign_ids).await?;

    let mut campaigns = Vec::with_capacity(rows.len());
    for (id, name, description, price, goal_count, created_at, _current_count, last_joined_at) in
        rows
    {
        let participants = participants_by_campaign
            .get(&id)
            .cloned()
            .unwrap_or_default();
        let current_count = participants.len() as i32;
        let status = if current_count >= goal_count {
            "closed"
        } else {
            "open"
        };
        campaigns.push(CampaignRes {
            tags: tags_by_campaign.get(&id).cloned().unwrap_or_default(),
            participants,
            id,
            name,
            description,
            price,
            goal_count,
            current_count,
            status: status.to_string(),
            created_at,
            last_joined_at,
        });
    }

    let mut campaign_json = HashMap::with_capacity(campaigns.len());
    for campaign in &campaigns {
        campaign_json.insert(campaign.id.clone(), serialize_json(campaign)?);
    }
    *state.cache.campaign_json.write().await = campaign_json;

    let open_campaigns: Vec<CampaignRes> = campaigns
        .iter()
        .filter(|campaign| campaign.status == "open")
        .cloned()
        .collect();
    let list_cache = build_warmed_list_cache(&open_campaigns, &tag_ids_by_name)?;
    *state.cache.list_campaigns_json.write().await = list_cache;

    let image_rows: Vec<(String, Vec<u8>)> = sqlx::query_as("SELECT id, image FROM campaigns")
        .fetch_all(&state.pool)
        .await?;
    let mut image_cache = HashMap::with_capacity(image_rows.len());
    for (id, bytes) in image_rows {
        let etag = format!("\"{}\"", hex::encode(sha2::Sha256::digest(&bytes)));
        image_cache.insert(
            id,
            ImageCache {
                bytes: Arc::new(bytes),
                etag,
            },
        );
    }
    *state.cache.campaign_image.write().await = image_cache;

    Ok(())
}

fn build_warmed_list_cache(
    open_campaigns: &[CampaignRes],
    tag_ids_by_name: &HashMap<String, String>,
) -> Result<HashMap<String, Arc<Vec<u8>>>, AppError> {
    let mut list_cache = HashMap::new();
    insert_list_cache(&mut list_cache, open_campaigns, &[], &[])?;

    let mut tag_pairs: Vec<(&String, &String)> = tag_ids_by_name.iter().collect();
    tag_pairs.sort_by(|a, b| a.0.cmp(b.0));
    for i in 0..tag_pairs.len() {
        insert_list_cache(
            &mut list_cache,
            open_campaigns,
            &[tag_pairs[i].0.as_str()],
            &[tag_pairs[i].1.as_str()],
        )?;
        for j in i + 1..tag_pairs.len() {
            insert_list_cache(
                &mut list_cache,
                open_campaigns,
                &[tag_pairs[i].0.as_str(), tag_pairs[j].0.as_str()],
                &[tag_pairs[i].1.as_str(), tag_pairs[j].1.as_str()],
            )?;
            for k in j + 1..tag_pairs.len() {
                insert_list_cache(
                    &mut list_cache,
                    open_campaigns,
                    &[
                        tag_pairs[i].0.as_str(),
                        tag_pairs[j].0.as_str(),
                        tag_pairs[k].0.as_str(),
                    ],
                    &[
                        tag_pairs[i].1.as_str(),
                        tag_pairs[j].1.as_str(),
                        tag_pairs[k].1.as_str(),
                    ],
                )?;
            }
        }
    }
    Ok(list_cache)
}

fn insert_list_cache(
    list_cache: &mut HashMap<String, Arc<Vec<u8>>>,
    open_campaigns: &[CampaignRes],
    tag_names: &[&str],
    tag_ids: &[&str],
) -> Result<(), AppError> {
    let mut filtered: Vec<CampaignRes> = open_campaigns
        .iter()
        .filter(|campaign| {
            tag_names
                .iter()
                .all(|tag_name| campaign.tags.iter().any(|tag| tag == tag_name))
        })
        .cloned()
        .collect();

    filtered.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let latest: Vec<CampaignRes> = filtered.iter().take(30).cloned().collect();
    let tag_key = sorted_tag_key(tag_ids);
    list_cache.insert(format!("sort=new;tags={tag_key}"), serialize_json(&latest)?);

    filtered.sort_by(|a, b| {
        let ak = a.last_joined_at.unwrap_or(a.created_at);
        let bk = b.last_joined_at.unwrap_or(b.created_at);
        bk.cmp(&ak)
    });
    let active: Vec<CampaignRes> = filtered.iter().take(30).cloned().collect();
    list_cache.insert(
        format!("sort=active;tags={tag_key}"),
        serialize_json(&active)?,
    );
    Ok(())
}

fn sorted_tag_key(tag_ids: &[&str]) -> String {
    let mut sorted = tag_ids.to_vec();
    sorted.sort_unstable();
    sorted.join(",")
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
    let tag_ids = resolve_tag_ids(&state, &req.tags).await?;

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
    state.cache.clear_list_campaigns().await;
    state
        .cache
        .campaign_json
        .write()
        .await
        .insert(id.clone(), serialize_json(&res)?);
    let etag = format!("\"{}\"", hex::encode(sha2::Sha256::digest(&image_bytes)));
    state.cache.campaign_image.write().await.insert(
        id,
        ImageCache {
            bytes: Arc::new(image_bytes),
            etag,
        },
    );
    Ok((StatusCode::CREATED, Json(res)))
}

async fn get_campaign_image(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, AppError> {
    if let Some(image) = state.cache.campaign_image.read().await.get(&id).cloned() {
        return Ok(image_response(image, &headers));
    }

    let row: Option<(Vec<u8>,)> = sqlx::query_as("SELECT image FROM campaigns WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await?;
    let bytes = match row {
        Some((b,)) => b,
        None => return Err(AppError::NotFound),
    };
    let hash_hex = hex::encode(sha2::Sha256::digest(&bytes));
    let etag = format!("\"{hash_hex}\"");
    let image = ImageCache {
        bytes: Arc::new(bytes),
        etag,
    };
    state
        .cache
        .campaign_image
        .write()
        .await
        .insert(id, image.clone());
    Ok(image_response(image, &headers))
}

fn image_response(image: ImageCache, headers: &HeaderMap) -> Response {
    let not_modified = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(|value| value == image.etag)
        .unwrap_or(false);
    if not_modified {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, image.etag)]).into_response();
    }

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/jpeg".to_string()),
            (header::ETAG, image.etag),
        ],
        Body::from(image.bytes.as_ref().clone()),
    )
        .into_response()
}

async fn get_campaign(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, AppError> {
    if let Some(body) = state.cache.campaign_json.read().await.get(&id).cloned() {
        return Ok(response_from_json_bytes(body));
    }

    let campaign = hydrate_campaign_via_pool(&state.pool, &id)
        .await?
        .ok_or(AppError::NotFound)?;
    let body = serialize_json(&campaign)?;
    state.cache.campaign_json.write().await.insert(id, body.clone());
    Ok(response_from_json_bytes(body))
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

    let (credit_limit,): (i32,) =
        sqlx::query_as("SELECT credit_limit FROM users WHERE id = ? FOR UPDATE")
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

    let (before_credit_used,): (i64,) = sqlx::query_as(
        "SELECT CAST(COALESCE(SUM(c.price), 0) AS SIGNED) \
         FROM campaign_participants cp \
         JOIN campaigns c ON c.id = cp.campaign_id \
         JOIN ( \
             SELECT campaign_id, COUNT(*) AS current_count \
             FROM campaign_participants \
             GROUP BY campaign_id \
         ) cc ON cc.campaign_id = c.id \
         WHERE cp.user_id = ? AND cc.current_count < c.goal_count",
    )
    .bind(user_id.to_string())
    .fetch_one(&mut *tx)
    .await?;
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
    let mut cache_clear_user_ids: HashSet<String> = HashSet::from([user_id.to_string()]);
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
        let part_rows: Vec<(String, String)> =
            sqlx::query_as("SELECT id, user_id FROM campaign_participants WHERE campaign_id = ?")
                .bind(&campaign_id)
                .fetch_all(&mut *tx)
                .await?;
        for (pid, uid) in part_rows {
            cache_clear_user_ids.insert(uid);
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

    // Single authority: safe to invalidate/update caches post-commit.
    state.cache.clear_list_campaigns().await;
    state.cache.clear_users(cache_clear_user_ids.iter().map(String::as_str)).await;
    state.cache.campaign_json.write().await
        .insert(campaign_id.clone(), serialize_json(&response_campaign)?);

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
            let message = WebhookMessage {
                url: webhook_url.clone(),
                body,
            };
            if let Err(e) = state.webhook_tx.try_send(message) {
                eprintln!("webhook queue full for user {uid}: {e}");
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
    let tag_ids = resolve_tag_ids(&state, &req.tags).await?;

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
) -> Result<Response, AppError> {
    let user_id_s = user_id.to_string();
    if let Some(body) = state.cache.charges_json.read().await.get(&user_id_s).cloned() {
        return Ok(response_from_json_bytes(body));
    }

    let rows: Vec<(String, NaiveDateTime, String, String, i32)> = sqlx::query_as(
        "SELECT ch.id, ch.created_at, c.id, c.name, c.price \
         FROM charges ch \
         JOIN campaign_participants cp ON ch.campaign_participant_id = cp.id \
         JOIN campaigns c ON cp.campaign_id = c.id \
         WHERE cp.user_id = ? \
         ORDER BY ch.created_at DESC",
    )
    .bind(&user_id_s)
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
    let body = serialize_json(&res)?;
    state.cache.charges_json.write().await.insert(user_id_s, body.clone());
    Ok(response_from_json_bytes(body))
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

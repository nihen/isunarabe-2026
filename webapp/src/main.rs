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
use sqlx::mysql::{MySqlPool, MySqlPoolOptions};
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex, RwLock};
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

const DEFAULT_CREDIT_LIMIT: i32 = 60000;

// ── AppState ──

#[derive(Clone)]
struct AppState {
    pool: MySqlPool,
    sql_dir: PathBuf,
    image_dir: PathBuf,
    seed_image_dir: PathBuf,
    db: Arc<DbConn>,
    store: Arc<Store>,
    webhook_tx: mpsc::Sender<WebhookMessage>,
    replica_urls: Arc<Vec<String>>,
    http: reqwest::Client,
}

#[derive(Clone, Debug)]
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

// ── In-memory store ──

struct Store {
    users: RwLock<HashMap<String, MemUser>>,
    campaigns: RwLock<HashMap<String, MemCampaign>>,
    tags: RwLock<Vec<String>>,
    tag_id_by_name: RwLock<HashMap<String, String>>,
    tag_name_by_id: RwLock<HashMap<String, String>>,
    saved_searches: RwLock<HashMap<String, Vec<MemSavedSearch>>>,
    charges: RwLock<HashMap<String, Vec<ChargeEntry>>>,
    webhook_url: RwLock<String>,
    campaign_image: RwLock<HashMap<String, ImageCache>>,
    write_lock: Mutex<()>,
    // Pre-computed list responses (rebuilt on join/create, not on every list request)
    list_cache_dirty: RwLock<bool>,
    list_cache: RwLock<HashMap<String, Arc<Vec<u8>>>>,
    // Pre-serialized campaign responses
    campaign_json_cache: RwLock<HashMap<String, Arc<Vec<u8>>>>,
}

#[derive(Clone)]
struct MemUser {
    name: String,
    credit_limit: i32,
    open_credit_used: i32,
}

#[derive(Clone)]
struct MemCampaign {
    name: String,
    description: String,
    price: i32,
    goal_count: i32,
    created_at: NaiveDateTime,
    tags: Vec<String>,
    tag_ids: Vec<String>,
    participants: Vec<ParticipantRes>,
    participant_user_ids: HashSet<String>,
    status: String,
    last_joined_at: Option<NaiveDateTime>,
}

impl MemCampaign {
    fn current_count(&self) -> i32 {
        self.participants.len() as i32
    }

    fn to_response(&self, id: &str) -> CampaignRes {
        let cc = self.current_count();
        CampaignRes {
            id: id.to_string(),
            name: self.name.clone(),
            description: self.description.clone(),
            price: self.price,
            goal_count: self.goal_count,
            current_count: cc,
            tags: self.tags.clone(),
            status: if cc >= self.goal_count { "closed" } else { "open" }.to_string(),
            created_at: self.created_at,
            last_joined_at: self.last_joined_at,
            participants: self.participants.clone(),
        }
    }
}

// ── Sync events for replica ──

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum SyncEvent {
    Reload,
    UserCreated { id: String, name: String, credit_limit: i32 },
    CampaignJoined {
        campaign_id: String,
        user_id: String,
        user_name: String,
        joined_at: NaiveDateTime,
        price: i32,
        closed: bool,
        close_participant_ids: Vec<String>,
        new_charges: Vec<SyncCharge>,
    },
    CampaignCreated {
        id: String,
        name: String,
        description: String,
        price: i32,
        goal_count: i32,
        created_at: NaiveDateTime,
        tags: Vec<String>,
        tag_ids: Vec<String>,
    },
    SavedSearchCreated { user_id: String, tag_ids: Vec<String> },
}

#[derive(Serialize, Deserialize, Clone)]
struct SyncCharge {
    user_id: String,
    charge_id: String,
    campaign_id: String,
    campaign_name: String,
    campaign_price: i32,
    created_at: NaiveDateTime,
}

#[derive(Clone)]
struct MemSavedSearch {
    tag_ids: HashSet<String>,
}

#[derive(Clone)]
struct ChargeEntry {
    id: String,
    amount: i32,
    campaign_id: String,
    campaign_name: String,
    campaign_price: i32,
    created_at: NaiveDateTime,
}

#[derive(Clone)]
struct ImageCache {
    path: PathBuf,
    etag: String,
}

impl Store {
    fn new() -> Self {
        Store {
            users: RwLock::new(HashMap::new()),
            campaigns: RwLock::new(HashMap::new()),
            tags: RwLock::new(Vec::new()),
            tag_id_by_name: RwLock::new(HashMap::new()),
            tag_name_by_id: RwLock::new(HashMap::new()),
            saved_searches: RwLock::new(HashMap::new()),
            charges: RwLock::new(HashMap::new()),
            webhook_url: RwLock::new(String::new()),
            campaign_image: RwLock::new(HashMap::new()),
            write_lock: Mutex::new(()),
            list_cache_dirty: RwLock::new(true),
            list_cache: RwLock::new(HashMap::new()),
            campaign_json_cache: RwLock::new(HashMap::new()),
        }
    }

    async fn load_from_db(&self, pool: &MySqlPool) -> Result<(), AppError> {
        // Tags
        let tag_rows: Vec<(String, String)> =
            sqlx::query_as("SELECT id, name FROM tags").fetch_all(pool).await?;
        let mut tags = Vec::new();
        let mut tag_id_by_name = HashMap::new();
        let mut tag_name_by_id = HashMap::new();
        for (id, name) in tag_rows {
            tags.push(name.clone());
            tag_id_by_name.insert(name.clone(), id.clone());
            tag_name_by_id.insert(id, name);
        }
        *self.tags.write().await = tags;
        *self.tag_id_by_name.write().await = tag_id_by_name;
        *self.tag_name_by_id.write().await = tag_name_by_id;

        // Users
        let user_rows: Vec<(String, String, i32)> =
            sqlx::query_as("SELECT id, name, credit_limit FROM users")
                .fetch_all(pool).await?;
        let mut users = HashMap::with_capacity(user_rows.len());
        for (id, name, credit_limit) in user_rows {
            users.insert(id, MemUser { name, credit_limit, open_credit_used: 0 });
        }

        // Campaigns (without image)
        let camp_rows: Vec<(String, String, String, i32, i32, NaiveDateTime)> =
            sqlx::query_as(
                "SELECT id, name, description, price, goal_count, created_at FROM campaigns",
            )
            .fetch_all(pool).await?;
        let mut campaigns: HashMap<String, MemCampaign> = HashMap::with_capacity(camp_rows.len());
        for (id, name, description, price, goal_count, created_at) in camp_rows {
            campaigns.insert(id, MemCampaign {
                name, description, price, goal_count, created_at,
                tags: Vec::new(), tag_ids: Vec::new(),
                participants: Vec::new(), participant_user_ids: HashSet::new(),
                status: "open".to_string(), last_joined_at: None,
            });
        }

        // Campaign tags
        let ct_rows: Vec<(String, String)> =
            sqlx::query_as("SELECT campaign_id, tag_id FROM campaign_tags")
                .fetch_all(pool).await?;
        let tag_name_map = self.tag_name_by_id.read().await;
        for (cid, tid) in ct_rows {
            if let Some(camp) = campaigns.get_mut(&cid) {
                if let Some(name) = tag_name_map.get(&tid) {
                    camp.tags.push(name.clone());
                }
                camp.tag_ids.push(tid);
            }
        }
        drop(tag_name_map);

        // Participants
        let part_rows: Vec<(String, String, String, NaiveDateTime)> = sqlx::query_as(
            "SELECT cp.campaign_id, cp.user_id, u.name, cp.created_at \
             FROM campaign_participants cp JOIN users u ON cp.user_id = u.id \
             ORDER BY cp.created_at ASC",
        )
        .fetch_all(pool).await?;
        for (cid, uid, uname, joined_at) in part_rows {
            if let Some(camp) = campaigns.get_mut(&cid) {
                camp.participants.push(ParticipantRes {
                    user_id: uid.clone(), name: uname, joined_at,
                });
                camp.participant_user_ids.insert(uid);
                camp.last_joined_at = Some(joined_at);
            }
        }

        // Finalize campaign status + compute open_credit_used
        for (_, camp) in campaigns.iter_mut() {
            if camp.current_count() >= camp.goal_count {
                camp.status = "closed".to_string();
            }
        }
        for (cid, camp) in &campaigns {
            if camp.status == "open" {
                for p in &camp.participants {
                    if let Some(user) = users.get_mut(&p.user_id) {
                        user.open_credit_used += camp.price;
                    }
                }
            }
        }

        *self.users.write().await = users;
        *self.campaigns.write().await = campaigns;

        // Charges
        let charge_rows: Vec<(String, NaiveDateTime, String, String, String, i32)> =
            sqlx::query_as(
                "SELECT ch.id, ch.created_at, cp.user_id, c.id, c.name, c.price \
                 FROM charges ch \
                 JOIN campaign_participants cp ON ch.campaign_participant_id = cp.id \
                 JOIN campaigns c ON cp.campaign_id = c.id \
                 ORDER BY ch.created_at DESC",
            )
            .fetch_all(pool).await?;
        let mut charges: HashMap<String, Vec<ChargeEntry>> = HashMap::new();
        for (id, created_at, uid, cid, cname, cprice) in charge_rows {
            charges.entry(uid).or_default().push(ChargeEntry {
                id, amount: cprice, campaign_id: cid, campaign_name: cname,
                campaign_price: cprice, created_at,
            });
        }
        *self.charges.write().await = charges;

        // Saved searches
        let ss_rows: Vec<(String, String)> =
            sqlx::query_as("SELECT id, user_id FROM saved_searches")
                .fetch_all(pool).await?;
        let sst_rows: Vec<(String, String)> =
            sqlx::query_as("SELECT saved_search_id, tag_id FROM saved_search_tags")
                .fetch_all(pool).await?;
        let mut ss_tags: HashMap<String, HashSet<String>> = HashMap::new();
        for (ssid, tid) in sst_rows {
            ss_tags.entry(ssid).or_default().insert(tid);
        }
        let mut saved_searches: HashMap<String, Vec<MemSavedSearch>> = HashMap::new();
        for (ssid, uid) in ss_rows {
            let tag_ids = ss_tags.remove(&ssid).unwrap_or_default();
            saved_searches.entry(uid).or_default().push(MemSavedSearch { tag_ids });
        }
        *self.saved_searches.write().await = saved_searches;

        // Webhook URL
        let url_row: Option<(String,)> = sqlx::query_as(
            "SELECT value FROM app_config WHERE name = 'notification_webhook_url'",
        )
        .fetch_optional(pool).await?;
        *self.webhook_url.write().await = url_row.map(|(v,)| v).unwrap_or_default();

        // Clear image cache (will warm lazily)
        self.campaign_image.write().await.clear();

        // Rebuild list cache + campaign JSON cache
        *self.list_cache_dirty.write().await = true;
        self.campaign_json_cache.write().await.clear();
        self.rebuild_list_cache_if_dirty().await;

        Ok(())
    }

    async fn rebuild_list_cache_if_dirty(&self) {
        {
            let dirty = self.list_cache_dirty.read().await;
            if !*dirty { return; }
        }

        let campaigns = self.campaigns.read().await;
        let tag_map = self.tag_id_by_name.read().await;

        let open: Vec<(String, &MemCampaign)> = campaigns.iter()
            .filter(|(_, c)| c.status == "open")
            .map(|(id, c)| (id.clone(), c))
            .collect();

        let mut cache = HashMap::new();
        let tag_names: Vec<&String> = tag_map.keys().collect();

        // Build all tag combinations (0, 1, 2, 3 tags)
        let mut tag_combos: Vec<Vec<&str>> = vec![vec![]]; // empty = no filter
        for name in &tag_names {
            tag_combos.push(vec![name.as_str()]);
        }
        for i in 0..tag_names.len() {
            for j in i+1..tag_names.len() {
                tag_combos.push(vec![tag_names[i].as_str(), tag_names[j].as_str()]);
                for k in j+1..tag_names.len() {
                    tag_combos.push(vec![tag_names[i].as_str(), tag_names[j].as_str(), tag_names[k].as_str()]);
                }
            }
        }

        for combo in &tag_combos {
            let filtered: Vec<CampaignRes> = open.iter()
                .filter(|(_, c)| combo.iter().all(|tag| c.tags.iter().any(|t| t == tag)))
                .map(|(id, c)| c.to_response(id))
                .collect();

            // sort=new
            let mut by_new = filtered.clone();
            by_new.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            by_new.truncate(30);

            // sort=active
            let mut by_active = filtered;
            by_active.sort_by(|a, b| {
                let ak = a.last_joined_at.unwrap_or(a.created_at);
                let bk = b.last_joined_at.unwrap_or(b.created_at);
                bk.cmp(&ak)
            });
            by_active.truncate(30);

            let mut tag_ids: Vec<&str> = combo.iter()
                .filter_map(|name| tag_map.get(*name).map(|id| id.as_str()))
                .collect();
            tag_ids.sort();
            let tag_key = tag_ids.join(",");

            let new_bytes = serde_json::to_vec(&by_new).unwrap_or_default();
            let active_bytes = serde_json::to_vec(&by_active).unwrap_or_default();
            cache.insert(format!("sort=new;tags={tag_key}"), Arc::new(new_bytes));
            cache.insert(format!("sort=active;tags={tag_key}"), Arc::new(active_bytes));
        }

        drop(campaigns);
        drop(tag_map);

        *self.list_cache.write().await = cache;
        *self.list_cache_dirty.write().await = false;
    }

    async fn invalidate_campaign(&self, campaign_id: &str, camp: &MemCampaign) {
        let bytes = serde_json::to_vec(&camp.to_response(campaign_id)).unwrap_or_default();
        self.campaign_json_cache.write().await
            .insert(campaign_id.to_string(), Arc::new(bytes));
        self.list_cache.write().await.clear();
    }
}

// ── main ──

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
    let image_dir = std::env::var("IMAGE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("nrb2026-webapp-images"));
    let seed_image_dir = std::env::var("SEED_IMAGE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| image_dir.join("seed"));

    let (webhook_tx, webhook_rx) = mpsc::channel(8192);
    let http = reqwest::Client::new();
    tokio::spawn(webhook_worker(http.clone(), webhook_rx));

    let replica_urls: Vec<String> = std::env::var("REPLICA_URLS")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.trim().to_string())
        .collect();

    let store = Arc::new(Store::new());
    if let Err(e) = store.load_from_db(&pool).await {
        eprintln!("startup load_from_db failed (expected on first boot): {e:?}");
    }

    let state = AppState {
        pool, sql_dir, image_dir, seed_image_dir,
        db: Arc::new(db),
        store,
        webhook_tx,
        replica_urls: Arc::new(replica_urls),
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
        .route_layer(middleware::from_fn_with_state(state.clone(), auth_middleware));

    let api = unauthed_api.merge(authed_api)
        .fallback(|| async { StatusCode::NOT_FOUND });

    let internal = Router::new()
        .route("/sync", post(handle_sync));

    let static_dir: Option<PathBuf> = std::env::var_os("STATIC_DIR").map(PathBuf::from);
    let mut app = Router::<AppState>::new()
        .route("/healthz", get(healthz))
        .nest("/api", api)
        .nest("/internal", internal);
    if let Some(dir) = static_dir {
        let index = dir.join("index.html");
        if !index.is_file() {
            panic!("STATIC_DIR index.html not found: {}", index.display());
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
    let listener = tokio::net::TcpListener::bind(addr).await
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
    while let Some(msg) = rx.recv().await {
        if let Err(e) = http.post(&msg.url).json(&msg.body).send().await {
            eprintln!("webhook send to {}: {e}", msg.url);
        }
    }
}

// ── Error / helpers ──

#[derive(Debug)]
enum AppError {
    Unauthorized, BadRequest, PaymentRequired, NotFound, Conflict, PayloadTooLarge,
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
            AppError::Internal(ref msg) => { eprintln!("internal error: {msg}"); StatusCode::INTERNAL_SERVER_ERROR }
        };
        (status, "").into_response()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self { AppError::Internal(format!("sqlx: {e}")) }
}

struct JsonReq<T>(T);

#[axum::async_trait]
impl<T, S> FromRequest<S> for JsonReq<T>
where T: serde::de::DeserializeOwned, S: Send + Sync,
{
    type Rejection = AppError;
    async fn from_request(req: Request, _state: &S) -> Result<Self, Self::Rejection> {
        let bytes = axum::body::to_bytes(req.into_body(), 1024 * 1024)
            .await.map_err(|_| AppError::BadRequest)?;
        let v: T = serde_json::from_slice(&bytes).map_err(|_| AppError::BadRequest)?;
        Ok(JsonReq(v))
    }
}

async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let header = req.headers().get("x-user-id")
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthorized)?;
    let user_id = Uuid::parse_str(header).map_err(|_| AppError::Unauthorized)?;
    let user_id_s = user_id.to_string();
    if !state.store.users.read().await.contains_key(&user_id_s) {
        return Err(AppError::Unauthorized);
    }
    req.extensions_mut().insert(AuthUser(user_id));
    Ok(next.run(req).await)
}

fn now_naive() -> NaiveDateTime { Utc::now().naive_utc() }

fn fmt_dt(dt: NaiveDateTime) -> String { dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string() }

fn serialize_dt<S: Serializer>(dt: &NaiveDateTime, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&fmt_dt(*dt))
}

fn serialize_dt_opt<S: Serializer>(dt: &Option<NaiveDateTime>, s: S) -> Result<S::Ok, S::Error> {
    match dt { Some(dt) => s.serialize_str(&fmt_dt(*dt)), None => s.serialize_none() }
}

fn validate_price(price: i32) -> Result<(), AppError> {
    if !(2000..=20000).contains(&price) { return Err(AppError::BadRequest); }
    Ok(())
}

fn validate_jpeg_image_b64(b64: &str) -> Result<Vec<u8>, AppError> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    let bytes = STANDARD.decode(b64).map_err(|_| AppError::BadRequest)?;
    if bytes.is_empty() { return Err(AppError::BadRequest); }
    if bytes.len() > 204_800 { return Err(AppError::PayloadTooLarge); }
    if bytes.len() < 3 || bytes[0] != 0xFF || bytes[1] != 0xD8 || bytes[2] != 0xFF {
        return Err(AppError::BadRequest);
    }
    Ok(bytes)
}

// ── Response types ──

#[derive(Clone, Serialize)]
struct CampaignRes {
    id: String, name: String, description: String, price: i32,
    goal_count: i32, current_count: i32, tags: Vec<String>, status: String,
    #[serde(serialize_with = "serialize_dt")]
    created_at: NaiveDateTime,
    #[serde(serialize_with = "serialize_dt_opt")]
    last_joined_at: Option<NaiveDateTime>,
    participants: Vec<ParticipantRes>,
}

#[derive(Clone, Serialize)]
struct ParticipantRes {
    user_id: String, name: String,
    #[serde(serialize_with = "serialize_dt")]
    joined_at: NaiveDateTime,
}

#[derive(Serialize)]
struct UserRes { id: String, name: String, credit_limit: i32 }

#[derive(Serialize)]
struct MeRes { id: String, name: String, credit_limit: i32, credit_used: i32 }

#[derive(Clone, Serialize)]
struct ChargeRes {
    id: String, amount: i32, campaign: ChargeCampaign,
    #[serde(serialize_with = "serialize_dt")]
    created_at: NaiveDateTime,
}

#[derive(Clone, Serialize)]
struct ChargeCampaign { id: String, name: String, price: i32 }

// ── Initialize ──

#[derive(Deserialize)]
struct InitReq { notification_webhook_url: String }

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
    .execute(&state.pool).await?;

    reset_image_dir(&state).await?;
    state.store.load_from_db(&state.pool).await?;

    // Tell replicas to reload (synchronous — must complete before bench starts)
    broadcast_sync(&state, &SyncEvent::Reload).await;

    Ok(Json(serde_json::json!({})))
}

async fn reset_image_dir(state: &AppState) -> Result<(), AppError> {
    match tokio::fs::remove_dir_all(&state.image_dir).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(AppError::Internal(format!("remove image dir: {e}"))),
    }
    tokio::fs::create_dir_all(&state.image_dir).await
        .map_err(|e| AppError::Internal(format!("create image dir: {e}")))?;
    Ok(())
}

async fn run_mysql_file(state: &AppState, path: &std::path::Path) -> Result<(), AppError> {
    let f = std::fs::File::open(path)
        .map_err(|e| AppError::Internal(format!("open {}: {e}", path.display())))?;
    let status = Command::new("mysql")
        .env("MYSQL_PWD", &state.db.password)
        .arg("-h").arg(&state.db.host)
        .arg("-P").arg(state.db.port.to_string())
        .arg("-u").arg(&state.db.user)
        .arg("--protocol=TCP")
        .arg("--default-character-set=utf8mb4")
        .arg(&state.db.database)
        .stdin(Stdio::from(f))
        .status().await
        .map_err(|e| AppError::Internal(format!("spawn mysql: {e}")))?;
    if !status.success() { return Err(AppError::Internal(format!("mysql exit {status}"))); }
    Ok(())
}

async fn healthz() -> StatusCode { StatusCode::OK }

// ── User APIs ──

#[derive(Deserialize)]
struct CreateUserReq { name: String }

async fn create_user(
    State(state): State<AppState>,
    JsonReq(req): JsonReq<CreateUserReq>,
) -> Result<Json<UserRes>, AppError> {
    let len = req.name.chars().count();
    if len == 0 || len > 100 { return Err(AppError::BadRequest); }
    let id = Uuid::new_v4().to_string();
    let now = now_naive();
    let credit_limit = DEFAULT_CREDIT_LIMIT;

    sqlx::query("INSERT INTO users (id, name, credit_limit, created_at) VALUES (?, ?, ?, ?)")
        .bind(&id).bind(&req.name).bind(credit_limit).bind(now)
        .execute(&state.pool).await?;

    let _guard = state.store.write_lock.lock().await;
    state.store.users.write().await.insert(id.clone(), MemUser {
        name: req.name.clone(), credit_limit, open_credit_used: 0,
    });
    drop(_guard);

    let event = SyncEvent::UserCreated { id: id.clone(), name: req.name.clone(), credit_limit };
    let st = state.clone();
    broadcast_sync(&state, &event).await;

    Ok(Json(UserRes { id, name: req.name, credit_limit }))
}

async fn get_me(
    State(state): State<AppState>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
) -> Result<Json<MeRes>, AppError> {
    let user_id_s = user_id.to_string();
    let users = state.store.users.read().await;
    let user = users.get(&user_id_s).ok_or(AppError::Unauthorized)?;
    Ok(Json(MeRes {
        id: user_id_s,
        name: user.name.clone(),
        credit_limit: user.credit_limit,
        credit_used: user.open_credit_used,
    }))
}

// ── Tags ──

async fn list_tags(State(state): State<AppState>) -> Result<Json<Vec<String>>, AppError> {
    let tags = state.store.tags.read().await;
    Ok(Json(tags.clone()))
}

// ── Campaigns ──

#[derive(Deserialize)]
struct ListCampaignsQuery { tags: Option<String>, sort: Option<String> }

fn response_from_json_bytes(body: Arc<Vec<u8>>) -> Response {
    (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")],
     Body::from(body.as_ref().clone())).into_response()
}

async fn list_campaigns(
    State(state): State<AppState>,
    AxumQuery(q): AxumQuery<ListCampaignsQuery>,
) -> Result<Response, AppError> {
    let mut tag_ids: Vec<String> = match q.tags.as_deref() {
        Some(s) if !s.is_empty() => {
            let parts: Vec<String> = s.split(',').map(|p| p.to_string()).collect();
            if parts.len() > 3 { return Err(AppError::BadRequest); }
            let mut seen = HashSet::new();
            for p in &parts {
                if !seen.insert(p.clone()) { return Err(AppError::BadRequest); }
            }
            let tag_map = state.store.tag_id_by_name.read().await;
            let mut ids = Vec::new();
            for p in &parts {
                ids.push(tag_map.get(p).ok_or(AppError::BadRequest)?.clone());
            }
            ids
        }
        _ => Vec::new(),
    };

    let sort_mode = match q.sort.as_deref() {
        Some("active") => "active",
        Some("new") | None => "new",
        _ => return Err(AppError::BadRequest),
    };

    tag_ids.sort();
    let cache_key = format!("sort={sort_mode};tags={}", tag_ids.join(","));

    // Check cache
    if let Some(body) = state.store.list_cache.read().await.get(&cache_key).cloned() {
        return Ok(response_from_json_bytes(body));
    }

    // Cache miss: compute just this key
    let filter_tag_names: Vec<String> = {
        let tag_name_map = state.store.tag_name_by_id.read().await;
        tag_ids.iter().filter_map(|id| tag_name_map.get(id).cloned()).collect()
    };
    let campaigns = state.store.campaigns.read().await;
    let mut open: Vec<CampaignRes> = campaigns.iter()
        .filter(|(_, c)| c.status == "open")
        .filter(|(_, c)| filter_tag_names.iter().all(|tag| c.tags.contains(tag)))
        .map(|(id, c)| c.to_response(id))
        .collect();
    drop(campaigns);
    if sort_mode == "active" {
        open.sort_by(|a, b| {
            let ak = a.last_joined_at.unwrap_or(a.created_at);
            let bk = b.last_joined_at.unwrap_or(b.created_at);
            bk.cmp(&ak)
        });
    } else {
        open.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    }
    open.truncate(30);
    let bytes = Arc::new(serde_json::to_vec(&open).unwrap_or_default());
    state.store.list_cache.write().await.insert(cache_key, bytes.clone());
    Ok(response_from_json_bytes(bytes))
}

async fn get_campaign(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, AppError> {
    // Check pre-serialized cache
    if let Some(body) = state.store.campaign_json_cache.read().await.get(&id) {
        return Ok(response_from_json_bytes(body.clone()));
    }
    let campaigns = state.store.campaigns.read().await;
    let camp = campaigns.get(&id).ok_or(AppError::NotFound)?;
    let res = camp.to_response(&id);
    drop(campaigns);
    let bytes = serde_json::to_vec(&res).map_err(|e| AppError::Internal(format!("json: {e}")))?;
    let body = Arc::new(bytes);
    state.store.campaign_json_cache.write().await.insert(id, body.clone());
    Ok(response_from_json_bytes(body))
}

// ── Campaign image ──

async fn get_campaign_image(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, AppError> {
    if let Some(image) = state.store.campaign_image.read().await.get(&id).cloned() {
        return image_response(image, &headers).await;
    }
    let image = load_campaign_image(&state, &id).await?;
    state.store.campaign_image.write().await.insert(id, image.clone());
    image_response(image, &headers).await
}

async fn load_campaign_image(state: &AppState, campaign_id: &str) -> Result<ImageCache, AppError> {
    let seed_path = state.seed_image_dir.join(format!("{campaign_id}.jpg"));
    if let Some(image) = image_cache_from_file(seed_path).await? { return Ok(image); }
    let dynamic_path = state.image_dir.join(format!("{campaign_id}.jpg"));
    if let Some(image) = image_cache_from_file(dynamic_path).await? { return Ok(image); }
    let row: Option<(Vec<u8>,)> = sqlx::query_as("SELECT image FROM campaigns WHERE id = ?")
        .bind(campaign_id).fetch_optional(&state.pool).await?;
    let bytes = match row { Some((b,)) => b, None => return Err(AppError::NotFound) };
    write_campaign_image_file(state, campaign_id, &bytes).await
}

async fn image_cache_from_file(path: PathBuf) -> Result<Option<ImageCache>, AppError> {
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(AppError::Internal(format!("read image: {e}"))),
    };
    let etag = format!("\"{}\"", hex::encode(sha2::Sha256::digest(&bytes)));
    Ok(Some(ImageCache { path, etag }))
}

async fn write_campaign_image_file(state: &AppState, id: &str, bytes: &[u8]) -> Result<ImageCache, AppError> {
    tokio::fs::create_dir_all(&state.image_dir).await
        .map_err(|e| AppError::Internal(format!("create image dir: {e}")))?;
    let path = state.image_dir.join(format!("{id}.jpg"));
    tokio::fs::write(&path, bytes).await
        .map_err(|e| AppError::Internal(format!("write image: {e}")))?;
    let etag = format!("\"{}\"", hex::encode(sha2::Sha256::digest(bytes)));
    Ok(ImageCache { path, etag })
}

async fn image_response(image: ImageCache, headers: &HeaderMap) -> Result<Response, AppError> {
    let not_modified = headers.get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v == image.etag).unwrap_or(false);
    if not_modified {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, image.etag)]).into_response());
    }
    let bytes = tokio::fs::read(&image.path).await
        .map_err(|e| AppError::Internal(format!("read image: {e}")))?;
    Ok((StatusCode::OK, [
        (header::CONTENT_TYPE, "image/jpeg".to_string()),
        (header::ETAG, image.etag),
    ], Body::from(bytes)).into_response())
}

// ── Create campaign ──

#[derive(Deserialize)]
struct CreateCampaignReq {
    name: String, description: String, price: i32, goal_count: i32,
    tags: Vec<String>, image: String,
}

async fn create_campaign(
    State(state): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    JsonReq(req): JsonReq<CreateCampaignReq>,
) -> Result<(StatusCode, Json<CampaignRes>), AppError> {
    let name_len = req.name.chars().count();
    if name_len == 0 || name_len > 100 { return Err(AppError::BadRequest); }
    let desc_len = req.description.chars().count();
    if desc_len == 0 || desc_len > 1000 { return Err(AppError::BadRequest); }
    validate_price(req.price)?;
    if req.goal_count < 2 || req.goal_count > 20 { return Err(AppError::BadRequest); }
    if req.tags.len() > 10 { return Err(AppError::BadRequest); }
    let mut seen_names = HashSet::new();
    for t in &req.tags {
        if !seen_names.insert(t.clone()) { return Err(AppError::BadRequest); }
    }
    let image_bytes = validate_jpeg_image_b64(&req.image)?;

    let tag_map = state.store.tag_id_by_name.read().await;
    let mut tag_ids = Vec::new();
    for t in &req.tags {
        let tid = tag_map.get(t).ok_or(AppError::BadRequest)?;
        if tag_ids.contains(tid) { return Err(AppError::BadRequest); }
        tag_ids.push(tid.clone());
    }
    drop(tag_map);

    let id = Uuid::new_v4().to_string();
    let now = now_naive();

    // DB write (for persistence /追試)
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO campaigns (id, name, description, price, goal_count, image, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id).bind(&req.name).bind(&req.description).bind(req.price)
    .bind(req.goal_count).bind(&image_bytes).bind(now)
    .execute(&mut *tx).await?;
    for tid in &tag_ids {
        sqlx::query("INSERT INTO campaign_tags (campaign_id, tag_id, created_at) VALUES (?, ?, ?)")
            .bind(&id).bind(tid).bind(now)
            .execute(&mut *tx).await?;
    }
    tx.commit().await?;

    let camp = MemCampaign {
        name: req.name, description: req.description, price: req.price,
        goal_count: req.goal_count, created_at: now,
        tags: req.tags, tag_ids,
        participants: Vec::new(), participant_user_ids: HashSet::new(),
        status: "open".to_string(), last_joined_at: None,
    };
    let res = camp.to_response(&id);
    let camp_clone = camp.clone();

    let _guard = state.store.write_lock.lock().await;
    state.store.campaigns.write().await.insert(id.clone(), camp);
    drop(_guard);

    state.store.invalidate_campaign(&id, &camp_clone).await;

    let event = SyncEvent::CampaignCreated {
        id: id.clone(), name: camp_clone.name, description: camp_clone.description,
        price: camp_clone.price, goal_count: camp_clone.goal_count,
        created_at: camp_clone.created_at, tags: camp_clone.tags, tag_ids: camp_clone.tag_ids,
    };
    let st = state.clone();
    broadcast_sync(&state, &event).await;

    let image = write_campaign_image_file(&state, &id, &image_bytes).await?;
    state.store.campaign_image.write().await.insert(id, image);

    Ok((StatusCode::CREATED, Json(res)))
}

// ── Join campaign ──

#[derive(Deserialize)]
struct JoinReq {}

async fn join_campaign(
    State(state): State<AppState>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    AxumPath(campaign_id): AxumPath<String>,
    JsonReq(_): JsonReq<JoinReq>,
) -> Result<Json<CampaignRes>, AppError> {
    let user_id_s = user_id.to_string();
    let now = now_naive();

    let _guard = state.store.write_lock.lock().await;
    let mut campaigns = state.store.campaigns.write().await;
    let mut users = state.store.users.write().await;

    let camp = campaigns.get(&campaign_id).ok_or(AppError::NotFound)?;
    let user = users.get(&user_id_s).ok_or(AppError::Unauthorized)?;

    if camp.current_count() >= camp.goal_count { return Err(AppError::Conflict); }
    if camp.participant_user_ids.contains(&user_id_s) { return Err(AppError::Conflict); }
    if user.open_credit_used + camp.price > user.credit_limit {
        return Err(AppError::PaymentRequired);
    }

    let price = camp.price;
    let goal_count = camp.goal_count;
    let camp_tag_ids: HashSet<String> = camp.tag_ids.iter().cloned().collect();

    // Add participant
    let camp = campaigns.get_mut(&campaign_id).unwrap();
    camp.participants.push(ParticipantRes {
        user_id: user_id_s.clone(), name: user.name.clone(), joined_at: now,
    });
    camp.participant_user_ids.insert(user_id_s.clone());
    camp.last_joined_at = Some(now);

    let user = users.get_mut(&user_id_s).unwrap();
    user.open_credit_used += price;

    let after = camp.current_count();

    // Notification check (before potential close)
    let mut webhook_user_ids: Vec<String> = Vec::new();
    if after == goal_count - 1 {
        let ss = state.store.saved_searches.read().await;
        let mut seen = HashSet::new();
        for (uid, searches) in ss.iter() {
            for search in searches {
                if search.tag_ids.iter().all(|tid| camp_tag_ids.contains(tid)) {
                    if seen.insert(uid.clone()) {
                        webhook_user_ids.push(uid.clone());
                    }
                }
            }
        }
    }

    // Campaign close
    if after == goal_count {
        camp.status = "closed".to_string();
        let participant_uids: Vec<String> = camp.participant_user_ids.iter().cloned().collect();
        let camp_name = camp.name.clone();
        let mut charges = state.store.charges.write().await;
        for uid in &participant_uids {
            if let Some(u) = users.get_mut(uid) {
                u.open_credit_used -= price;
            }
            let charge = ChargeEntry {
                id: Uuid::new_v4().to_string(),
                amount: price,
                campaign_id: campaign_id.clone(),
                campaign_name: camp_name.clone(),
                campaign_price: price,
                created_at: now,
            };
            charges.entry(uid.clone()).or_default().insert(0, charge);
        }
    }

    let camp_snapshot = camp.clone();
    let response = camp.to_response(&campaign_id);
    let webhook_url = state.store.webhook_url.read().await.clone();

    drop(campaigns);
    drop(users);
    drop(_guard);

    // Update caches (outside write_lock)
    state.store.invalidate_campaign(&campaign_id, &camp_snapshot).await;

    // Sync to replicas
    {
        let closed = camp_snapshot.status == "closed";
        let close_ids: Vec<String> = if closed {
            camp_snapshot.participant_user_ids.iter().cloned().collect()
        } else { Vec::new() };
        let new_charges: Vec<SyncCharge> = if closed {
            let charges = state.store.charges.read().await;
            close_ids.iter().filter_map(|uid| {
                charges.get(uid).and_then(|cs| cs.first()).map(|c| SyncCharge {
                    user_id: uid.clone(), charge_id: c.id.clone(),
                    campaign_id: c.campaign_id.clone(), campaign_name: c.campaign_name.clone(),
                    campaign_price: c.campaign_price, created_at: c.created_at,
                })
            }).collect()
        } else { Vec::new() };

        let event = SyncEvent::CampaignJoined {
            campaign_id: campaign_id.clone(), user_id: user_id_s.clone(),
            user_name: { state.store.users.read().await.get(&user_id_s).map(|u| u.name.clone()).unwrap_or_default() },
            joined_at: now, price, closed,
            close_participant_ids: close_ids, new_charges,
        };
        broadcast_sync(&state, &event).await;
    }

    // Synchronous DB write (for persistence / 追試). Lock already released.
    let participant_id = Uuid::new_v4().to_string();
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO campaign_participants (id, campaign_id, user_id, created_at) VALUES (?, ?, ?, ?)",
    ).bind(&participant_id).bind(&campaign_id).bind(&user_id_s).bind(now)
        .execute(&mut *tx).await?;
    if after == goal_count {
        let parts: Vec<(String,)> = sqlx::query_as(
            "SELECT id FROM campaign_participants WHERE campaign_id = ?",
        ).bind(&campaign_id).fetch_all(&mut *tx).await?;
        for (pid,) in parts {
            sqlx::query(
                "INSERT INTO charges (id, campaign_participant_id, created_at) VALUES (?, ?, ?)",
            ).bind(Uuid::new_v4().to_string()).bind(pid).bind(now)
                .execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;

    // Webhooks
    if !webhook_user_ids.is_empty() && !webhook_url.is_empty() {
        for uid in &webhook_user_ids {
            let body = serde_json::json!({
                "type": "campaign_closing_soon",
                "user_id": uid,
                "campaign": {
                    "id": &response.id,
                    "name": &response.name,
                    "description": &response.description,
                    "price": response.price,
                    "goal_count": response.goal_count,
                    "current_count": response.current_count,
                    "tags": &response.tags,
                    "status": &response.status,
                    "created_at": fmt_dt(response.created_at),
                    "last_joined_at": response.last_joined_at.map(fmt_dt),
                }
            });
            let _ = state.webhook_tx.try_send(WebhookMessage { url: webhook_url.clone(), body });
        }
    }

    Ok(Json(response))
}

// ── Saved searches ──

#[derive(Deserialize)]
struct CreateSavedSearchReq { tags: Vec<String> }

async fn create_saved_search(
    State(state): State<AppState>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    JsonReq(req): JsonReq<CreateSavedSearchReq>,
) -> Result<StatusCode, AppError> {
    if req.tags.is_empty() || req.tags.len() > 3 { return Err(AppError::BadRequest); }
    let mut seen_names = HashSet::new();
    for t in &req.tags {
        if !seen_names.insert(t.clone()) { return Err(AppError::BadRequest); }
    }

    let tag_map = state.store.tag_id_by_name.read().await;
    let mut tag_ids = HashSet::new();
    let mut tag_id_vec = Vec::new();
    for t in &req.tags {
        let tid = tag_map.get(t).ok_or(AppError::BadRequest)?;
        if !tag_ids.insert(tid.clone()) { return Err(AppError::BadRequest); }
        tag_id_vec.push(tid.clone());
    }
    drop(tag_map);

    let user_id_s = user_id.to_string();
    let _guard = state.store.write_lock.lock().await;
    let mut ss = state.store.saved_searches.write().await;
    let user_searches = ss.entry(user_id_s.clone()).or_default();
    if user_searches.len() >= 10 { return Err(AppError::Conflict); }
    user_searches.push(MemSavedSearch { tag_ids });
    drop(ss);
    drop(_guard);

    // Synchronous DB write (for persistence / 追試)
    let ss_id = Uuid::new_v4().to_string();
    let now = now_naive();
    let mut tx = state.pool.begin().await?;
    sqlx::query("INSERT INTO saved_searches (id, user_id, created_at) VALUES (?, ?, ?)")
        .bind(&ss_id).bind(&user_id_s).bind(now)
        .execute(&mut *tx).await?;
    for tid in &tag_id_vec {
        sqlx::query(
            "INSERT INTO saved_search_tags (saved_search_id, tag_id, created_at) VALUES (?, ?, ?)",
        ).bind(&ss_id).bind(tid).bind(now)
            .execute(&mut *tx).await?;
    }
    tx.commit().await?;

    let event = SyncEvent::SavedSearchCreated {
        user_id: user_id_s, tag_ids: tag_id_vec.into_iter().collect(),
    };
    let st = state.clone();
    broadcast_sync(&state, &event).await;

    Ok(StatusCode::CREATED)
}

// ── Charges ──

async fn list_charges(
    State(state): State<AppState>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
) -> Result<Json<Vec<ChargeRes>>, AppError> {
    let user_id_s = user_id.to_string();
    let charges = state.store.charges.read().await;
    let user_charges = charges.get(&user_id_s).cloned().unwrap_or_default();
    let res: Vec<ChargeRes> = user_charges.into_iter().map(|c| ChargeRes {
        id: c.id, amount: c.amount,
        campaign: ChargeCampaign { id: c.campaign_id, name: c.campaign_name, price: c.campaign_price },
        created_at: c.created_at,
    }).collect();
    Ok(Json(res))
}

// ── Internal sync ──

async fn handle_sync(
    State(state): State<AppState>,
    Json(event): Json<SyncEvent>,
) -> StatusCode {
    match apply_sync_event(&state, event).await {
        Ok(_) => StatusCode::OK,
        Err(e) => { eprintln!("sync apply error: {e:?}"); StatusCode::INTERNAL_SERVER_ERROR }
    }
}

async fn apply_sync_event(state: &AppState, event: SyncEvent) -> Result<(), AppError> {
    match event {
        SyncEvent::Reload => {
            state.store.load_from_db(&state.pool).await?;
        }
        SyncEvent::UserCreated { id, name, credit_limit } => {
            let _g = state.store.write_lock.lock().await;
            state.store.users.write().await.insert(id, MemUser {
                name, credit_limit, open_credit_used: 0,
            });
        }
        SyncEvent::CampaignCreated { id, name, description, price, goal_count, created_at, tags, tag_ids } => {
            let camp = MemCampaign {
                name, description, price, goal_count, created_at, tags, tag_ids,
                participants: Vec::new(), participant_user_ids: HashSet::new(),
                status: "open".to_string(), last_joined_at: None,
            };
            let _g = state.store.write_lock.lock().await;
            state.store.invalidate_campaign(&id, &camp).await;
            state.store.campaigns.write().await.insert(id, camp);
        }
        SyncEvent::CampaignJoined {
            campaign_id, user_id, user_name, joined_at, price,
            closed, close_participant_ids, new_charges,
        } => {
            let _g = state.store.write_lock.lock().await;
            let mut campaigns = state.store.campaigns.write().await;
            let mut users = state.store.users.write().await;

            if let Some(camp) = campaigns.get_mut(&campaign_id) {
                if !camp.participant_user_ids.contains(&user_id) {
                    camp.participants.push(ParticipantRes {
                        user_id: user_id.clone(), name: user_name, joined_at,
                    });
                    camp.participant_user_ids.insert(user_id.clone());
                    camp.last_joined_at = Some(joined_at);
                    if let Some(u) = users.get_mut(&user_id) {
                        u.open_credit_used += price;
                    }
                }
                if closed {
                    camp.status = "closed".to_string();
                    for uid in &close_participant_ids {
                        if let Some(u) = users.get_mut(uid) {
                            u.open_credit_used -= price;
                        }
                    }
                }
            }

            if !new_charges.is_empty() {
                let mut charges = state.store.charges.write().await;
                for sc in new_charges {
                    charges.entry(sc.user_id).or_default().insert(0, ChargeEntry {
                        id: sc.charge_id, amount: sc.campaign_price,
                        campaign_id: sc.campaign_id, campaign_name: sc.campaign_name,
                        campaign_price: sc.campaign_price, created_at: sc.created_at,
                    });
                }
            }

            if let Some(camp) = campaigns.get(&campaign_id) {
                let camp_clone = camp.clone();
                drop(campaigns);
                drop(users);
                state.store.invalidate_campaign(&campaign_id, &camp_clone).await;
            }
        }
        SyncEvent::SavedSearchCreated { user_id, tag_ids } => {
            let _g = state.store.write_lock.lock().await;
            state.store.saved_searches.write().await
                .entry(user_id).or_default()
                .push(MemSavedSearch { tag_ids: tag_ids.into_iter().collect() });
        }
    }
    Ok(())
}

async fn broadcast_sync(state: &AppState, event: &SyncEvent) {
    if state.replica_urls.is_empty() { return; }
    let body = match serde_json::to_vec(event) {
        Ok(b) => b,
        Err(_) => return,
    };
    for url in state.replica_urls.iter() {
        let target = format!("{url}/internal/sync");
        for attempt in 0..3 {
            match state.http.post(&target)
                .header("content-type", "application/json")
                .body(body.clone())
                .send().await
            {
                Ok(resp) if resp.status().is_success() => break,
                Ok(resp) => {
                    eprintln!("sync to {target}: HTTP {} (attempt {attempt})", resp.status());
                    if attempt == 2 { eprintln!("sync FAILED after 3 attempts"); }
                }
                Err(e) => {
                    eprintln!("sync to {target}: {e} (attempt {attempt})");
                    if attempt == 2 { eprintln!("sync FAILED after 3 attempts"); }
                }
            }
        }
    }
}

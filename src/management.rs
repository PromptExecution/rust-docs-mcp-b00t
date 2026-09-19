// Management API for multi-crate cloud deployment
// Provides HTTP endpoints to load/query crate doc servers on demand.
// Each crate gets its own MCP SSE server on an ephemeral port.

use crate::doc_loader::{self, Document};
use crate::embeddings::{generate_embeddings, CachedDocumentEmbedding, EMBEDDING_API_BASE, HTTP_CLIENT, OPENAI_CLIENT};
use crate::error::ServerError;
use crate::server::RustDocsServer;
use axum::{
    Json, Router,
    extract::Path,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use axum::body::Body;
use axum::extract::Query;
use axum::response::Response;
use futures::TryStreamExt;
use ndarray::Array1;
use rmcp::transport::sse_server::SseServer;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    env,
    fs::{self, File},
    io::BufReader,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};
use tokio::sync::RwLock;
use bincode::config as bincode_config;

#[cfg(not(target_os = "windows"))]
use xdg::BaseDirectories;

// --- Crate instance state ---

#[derive(Debug, Clone, Serialize)]
pub struct CrateInstance {
    pub crate_spec: String,
    pub port: u16,
    pub status: String,
    pub loaded_docs: usize,
    pub loaded_embeddings: usize,
}

#[derive(Clone)]
pub struct CrateRegistry {
    instances: Arc<RwLock<HashMap<String, CrateInstance>>>,
    port_allocator: Arc<RwLock<u16>>,
    management_port: u16,
}

impl CrateRegistry {
    pub fn new(management_port: u16, start_port: u16) -> Self {
        Self {
            instances: Arc::new(RwLock::new(HashMap::new())),
            port_allocator: Arc::new(RwLock::new(start_port)),
            management_port,
        }
    }

    async fn allocate_port(&self) -> u16 {
        let mut port = self.port_allocator.write().await;
        let p = *port;
        *port += 1;
        p
    }
}

// --- Request/Response types ---

#[derive(Deserialize)]
pub struct LoadCrateRequest {
    pub crate_spec: String,
    #[serde(default)]
    pub features: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct LoadCrateResponse {
    pub crate_spec: String,
    pub mcp_sse_url: String,
    pub mcp_message_url: String,
    pub port: u16,
    pub status: String,
    pub cached: bool,
}

#[derive(Serialize)]
pub struct CrateStatusResponse {
    pub crate_spec: String,
    pub port: u16,
    pub status: String,
    pub loaded_docs: usize,
    pub loaded_embeddings: usize,
}

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub active_crates: usize,
    pub management_port: u16,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

// --- Embedding cache helpers ---

fn embeddings_cache_path(
    crate_name: &str,
    version_req: &str,
    features: &Option<Vec<String>>,
) -> Result<PathBuf, ServerError> {
    let features_hash = hash_features(features);
    let sanitized_version = version_req
        .replace(|c: char| !c.is_alphanumeric() && c != '.' && c != '-', "_");
    let relative = PathBuf::from(crate_name)
        .join(&sanitized_version)
        .join(&features_hash)
        .join("embeddings.bin");

    #[cfg(not(target_os = "windows"))]
    {
        let xdg_dirs = BaseDirectories::with_prefix("rustdocs-mcp-server")
            .map_err(|e| ServerError::Xdg(format!("Failed to get XDG directories: {}", e)))?;
        Ok(xdg_dirs.get_data_home().join(relative))
    }
    #[cfg(target_os = "windows")]
    {
        let cache_dir = dirs::cache_dir().ok_or_else(|| {
            ServerError::Config("Could not determine cache directory".to_string())
        })?;
        Ok(cache_dir.join("rustdocs-mcp-server").join(relative))
    }
}

fn hash_features(features: &Option<Vec<String>>) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    features
        .as_ref()
        .map(|f| {
            let mut sorted = f.clone();
            sorted.sort_unstable();
            let mut hasher = DefaultHasher::new();
            sorted.hash(&mut hasher);
            format!("{:x}", hasher.finish())
        })
        .unwrap_or_else(|| "no_features".to_string())
}

fn try_load_cached(
    cache_path: &PathBuf,
) -> Option<(Vec<Document>, Vec<(String, Array1<f32>)>)> {
    if !cache_path.exists() {
        return None;
    }
    let file = File::open(cache_path).ok()?;
    let reader = BufReader::new(file);
    let cached: Vec<CachedDocumentEmbedding> =
        bincode::decode_from_reader(reader, bincode_config::standard()).ok()?;

    if cached.is_empty() {
        return None;
    }

    let mut documents = Vec::with_capacity(cached.len());
    let mut embeddings = Vec::with_capacity(cached.len());
    for item in cached {
        documents.push(Document {
            path: item.path.clone(),
            content: item.content,
        });
        embeddings.push((item.path, Array1::from(item.vector)));
    }
    Some((documents, embeddings))
}

fn save_cache(
    cache_path: &PathBuf,
    documents: &[Document],
    embeddings: &[(String, Array1<f32>)],
) {
    let embedding_map: HashMap<String, Array1<f32>> = embeddings.iter().cloned().collect();
    let mut combined = Vec::new();
    for doc in documents {
        if let Some(emb) = embedding_map.get(&doc.path) {
            combined.push(CachedDocumentEmbedding {
                path: doc.path.clone(),
                content: doc.content.clone(),
                vector: emb.to_vec(),
            });
        }
    }
    if let Some(parent) = cache_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(bytes) = bincode::encode_to_vec(&combined, bincode_config::standard()) {
        let _ = fs::write(cache_path, bytes);
    }
}

// Result of loading a crate (without the service, which is moved into SSE task)
struct LoadResult {
    doc_count: usize,
    emb_count: usize,
    from_cache: bool,
}


fn try_load_precache(
    crate_name: &str,
    version_req: &str,
    features: &Option<Vec<String>>,
) -> Option<Vec<Document>> {
    let features_hash = hash_features(features);
    let sanitized_version = version_req
        .replace(|c: char| !c.is_alphanumeric() && c != '.' && c != '-', "_");
    // Try exact version match first, then any version
    let precache_path = PathBuf::from("/precache")
        .join(crate_name)
        .join(&sanitized_version)
        .join(&features_hash)
        .join("docs.json");

    let actual_path = if precache_path.exists() {
        precache_path
    } else {
        // Scan /precache/{crate}/ for any version directory
        let crate_dir = PathBuf::from("/precache").join(crate_name);
        if !crate_dir.is_dir() { return None; }
        let mut found = None;
        if let Ok(entries) = std::fs::read_dir(&crate_dir) {
            for entry in entries.flatten() {
                let candidate = entry.path().join(&features_hash).join("docs.json");
                if candidate.exists() {
                    found = Some(candidate);
                    break;
                }
            }
        }
        found?
    };

    let data = std::fs::read_to_string(&actual_path).ok()?;
    let docs: Vec<Document> = serde_json::from_str(&data).ok()?;
    if docs.is_empty() {
        return None;
    }
    eprintln!("[{}] Loaded {} pre-cached docs from {}", crate_name, docs.len(), actual_path.display());
    Some(docs)
}

// --- Load a crate: generate docs + embeddings, start SSE server ---

async fn load_and_serve_crate(
    crate_spec: &str,
    features: &Option<Vec<String>>,
    port: u16,
    host: &str,
) -> Result<LoadResult, ServerError> {
    use cargo::core::PackageIdSpec;

    let spec = PackageIdSpec::parse(crate_spec).map_err(|e| {
        ServerError::Config(format!("Failed to parse crate spec '{}': {}", crate_spec, e))
    })?;
    let crate_name = spec.name().to_string();
    let version_req = spec
        .version()
        .map(|v| v.to_string())
        .unwrap_or_else(|| "*".to_string());

    let cache_path = embeddings_cache_path(&crate_name, &version_req, features)?;

    // Try cache first
    let (documents, embeddings, from_cache) =
        if let Some((docs, embs)) = try_load_cached(&cache_path) {
            eprintln!(
                "[{}] Loaded {} docs, {} embeddings from cache",
                crate_spec,
                docs.len(),
                embs.len()
            );
            (docs, embs, true)
        } else {
            eprintln!("[{}] Cache miss — checking pre-cache", crate_spec);
            let docs = try_load_precache(&crate_name, &version_req, features)
                .or_else(|| {
                    eprintln!("[{}] Pre-cache miss — generating docs via cargo doc", crate_spec);
                    doc_loader::load_documents(&crate_name, &version_req, features.as_ref()).ok()
                })
                .ok_or_else(|| ServerError::Config(format!("Failed to load docs for '{}' (no pre-cache, cargo doc failed — is rustc installed?)", crate_spec)))?;
            eprintln!("[{}] Loaded {} documents", crate_spec, docs.len());

            let _client = OPENAI_CLIENT
                .get()
                .ok_or_else(|| ServerError::Config("OpenAI client not initialized".to_string()))?;

            let embedding_model =
                env::var("EMBEDDING_MODEL").unwrap_or_else(|_| "text-embedding-3-small".to_string());
            let http_client = HTTP_CLIENT.get().ok_or_else(|| ServerError::Config("HTTP client not initialized".to_string()))?;
            let api_base = EMBEDDING_API_BASE.get().ok_or_else(|| ServerError::Config("Embedding API base not initialized".to_string()))?;
            let api_key = env::var("OPENAI_API_KEY").unwrap_or_default();
            let (embs, _tokens) = generate_embeddings(http_client, api_base, &api_key, &docs, &embedding_model).await?;
            eprintln!("[{}] Generated {} embeddings", crate_spec, embs.len());

            save_cache(&cache_path, &docs, &embs);
            (docs, embs, false)
        };

    let doc_count = documents.len();
    let emb_count = embeddings.len();

    let startup_msg = format!(
        "Server for '{}' initialized. {} docs, {} embeddings{}.",
        crate_spec,
        doc_count,
        emb_count,
        if from_cache { " (cached)" } else { " (fresh)" }
    );

    let service = RustDocsServer::new(crate_name, documents, embeddings, startup_msg)?;

    // Start SSE server on the allocated port
    let addr: SocketAddr = format!("{}:{}", host, port).parse().map_err(|e| {
        ServerError::Config(format!("Invalid bind address: {}", e))
    })?;

    let sse = SseServer::serve(addr).await.map_err(|e| {
        ServerError::Config(format!("Failed to start SSE server on port {}: {}", port, e))
    })?;

    // Service is moved into the SSE task — we don't need it back
    let _ct = sse.with_service(move || service.clone());
    eprintln!("[{}] SSE server listening on port {}", crate_spec, port);

    Ok(LoadResult {
        doc_count,
        emb_count,
        from_cache,
    })
}

// --- Axum route handlers ---

async fn handle_load_crate(
    axum::extract::State(registry): axum::extract::State<CrateRegistry>,
    Json(req): Json<LoadCrateRequest>,
) -> impl IntoResponse {
    let spec_key = req.crate_spec.clone();

    // Check if already loaded
    {
        let instances = registry.instances.read().await;
        if let Some(existing) = instances.get(&spec_key) {
            let base = format!("https://{}", resolve_host());
            return Json(LoadCrateResponse {
                crate_spec: spec_key,
                mcp_sse_url: format!("{}:{}/sse", base, existing.port),
                mcp_message_url: format!("{}:{}/message", base, existing.port),
                port: existing.port,
                status: existing.status.clone(),
                cached: true,
            })
            .into_response();
        }
    }

    let port = registry.allocate_port().await;
    let host = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());

    match load_and_serve_crate(&spec_key, &req.features, port, &host).await {
        Ok(result) => {
            let instance = CrateInstance {
                crate_spec: spec_key.clone(),
                port,
                status: "running".to_string(),
                loaded_docs: result.doc_count,
                loaded_embeddings: result.emb_count,
            };
            registry
                .instances
                .write()
                .await
                .insert(spec_key.clone(), instance);

            let base = format!("https://{}", resolve_host());
            Json(LoadCrateResponse {
                crate_spec: spec_key,
                mcp_sse_url: format!("{}:{}/sse", base, port),
                mcp_message_url: format!("{}:{}/message", base, port),
                port,
                status: "running".to_string(),
                cached: result.from_cache,
            })
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to load crate '{}': {}", spec_key, e),
            }),
        )
            .into_response(),
    }
}

async fn handle_list_crates(
    axum::extract::State(registry): axum::extract::State<CrateRegistry>,
) -> Json<Vec<CrateStatusResponse>> {
    let instances = registry.instances.read().await;
    let list: Vec<CrateStatusResponse> = instances
        .values()
        .map(|i| CrateStatusResponse {
            crate_spec: i.crate_spec.clone(),
            port: i.port,
            status: i.status.clone(),
            loaded_docs: i.loaded_docs,
            loaded_embeddings: i.loaded_embeddings,
        })
        .collect();
    Json(list)
}

async fn handle_crate_status(
    axum::extract::State(registry): axum::extract::State<CrateRegistry>,
    Path(crate_spec): Path<String>,
) -> impl IntoResponse {
    let instances = registry.instances.read().await;
    match instances.get(&crate_spec) {
        Some(i) => Json(CrateStatusResponse {
            crate_spec: i.crate_spec.clone(),
            port: i.port,
            status: i.status.clone(),
            loaded_docs: i.loaded_docs,
            loaded_embeddings: i.loaded_embeddings,
        })
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Crate '{}' is not loaded", crate_spec),
            }),
        )
            .into_response(),
    }
}

async fn handle_health(
    axum::extract::State(registry): axum::extract::State<CrateRegistry>,
) -> Json<HealthResponse> {
    let instances = registry.instances.read().await;
    Json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        active_crates: instances.len(),
        management_port: registry.management_port,
    })
}

fn resolve_host() -> String {
    let host = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    // For URLs, replace 0.0.0.0 with the actual hostname
    if host == "0.0.0.0" {
        env::var("PUBLIC_HOST")
            .unwrap_or_else(|_| "rust-docs.mcp.b00t.promptexecution.com".to_string())
    } else {
        host
    }
}

// --- SSE Proxy handlers ---

async fn proxy_sse(
    axum::extract::State(registry): axum::extract::State<CrateRegistry>,
    Path(crate_spec): Path<String>,
) -> impl IntoResponse {
    let instances = registry.instances.read().await;
    let instance = match instances.get(&crate_spec) {
        Some(i) => i,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: format!("Crate '{}' is not loaded. POST /api/crates first.", crate_spec),
                }),
            )
                .into_response();
        }
    };

    let target_url = format!("http://127.0.0.1:{}/sse", instance.port);
    let crate_spec_clone = crate_spec.clone();
    drop(instances);

    // Proxy the SSE request to the per-crate server, rewriting session URLs
    let client = reqwest::Client::new();
    match client.get(&target_url).send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let content_type = resp.headers().get("content-type")
                .map(|v| v.to_str().unwrap_or("text/event-stream").to_string())
                .unwrap_or_else(|| "text/event-stream".to_string());

            // Rewrite session URLs in the SSE stream so the client POSTs to our proxy
            let prefix = crate_spec_clone.clone();
            let body = resp.bytes_stream();
            use futures::StreamExt;
            let stream = body.map(move |chunk| {
                match chunk {
                    Ok(b) => {
                        let text = String::from_utf8_lossy(&b).to_string();
                        // Rewrite /message?sessionId= to /mcp/{crate}/message?sessionId=
                        let rewritten = text.replace(
                            "/message?sessionId=",
                            &format!("/mcp/{}/message?sessionId=", prefix),
                        ).replace(
                            "/message?session_id=",
                            &format!("/mcp/{}/message?session_id=", prefix),
                        );
                        Ok::<_, std::io::Error>(axum::body::Bytes::from(rewritten))
                    }
                    Err(e) => Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
                }
            });

            Response::builder()
                .status(status)
                .header("content-type", content_type)
                .header("cache-control", "no-cache")
                .header("connection", "keep-alive")
                .body(Body::from_stream(stream))
                .unwrap_or_else(|_| {
                    Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::empty())
                        .unwrap()
                })
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse {
                error: format!("Failed to proxy SSE to crate '{}': {}", crate_spec, e),
            }),
        )
            .into_response(),
    }
}

async fn proxy_message(
    axum::extract::State(registry): axum::extract::State<CrateRegistry>,
    Path(crate_spec): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    body: String,
) -> impl IntoResponse {
    let instances = registry.instances.read().await;
    let instance = match instances.get(&crate_spec) {
        Some(i) => i,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: format!("Crate '{}' is not loaded", crate_spec),
                }),
            )
                .into_response();
        }
    };

    let session_id = params.get("session_id").cloned().unwrap_or_default();
    let target_url = format!("http://127.0.0.1:{}/message?session_id={}", instance.port, session_id);
    drop(instances);

    let client = reqwest::Client::new();
    match client
        .post(&target_url)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let body_text = resp.text().await.unwrap_or_default();
            Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(Body::from(body_text))
                .unwrap_or_else(|_| {
                    Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::empty())
                        .unwrap()
                })
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse {
                error: format!("Failed to proxy message to crate '{}': {}", crate_spec, e),
            }),
        )
            .into_response(),
    }
}

// --- Main entry point for cloud mode ---

pub async fn run_cloud_server(management_port: u16) -> Result<(), ServerError> {
    let host = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let start_port = env::var("CRATE_PORT_START")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3001);

    let registry = CrateRegistry::new(management_port, start_port);

    // Pre-load crates from CRATE_PRELOAD env (comma-separated specs)
    if let Ok(preload) = env::var("CRATE_PRELOAD") {
        let specs: Vec<String> = preload
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        for spec in specs {
            let port = registry.allocate_port().await;
            eprintln!("Pre-loading crate '{}' on port {}...", spec, port);
            match load_and_serve_crate(&spec, &None, port, &host).await {
                Ok(result) => {
                    registry
                        .instances
                        .write()
                        .await
                        .insert(spec.clone(), CrateInstance {
                            crate_spec: spec.clone(),
                            port,
                            status: "running".to_string(),
                            loaded_docs: result.doc_count,
                            loaded_embeddings: result.emb_count,
                        });
                    eprintln!(
                        "  ✅ {} — {} docs, {} embeddings{}",
                        spec,
                        result.doc_count,
                        result.emb_count,
                        if result.from_cache { " (cached)" } else { "" }
                    );
                }
                Err(e) => {
                    eprintln!("  ❌ {} — {}", spec, e);
                }
            }
        }
    }

    let app = Router::new()
        .route("/health", get(handle_health))
        .route("/api/crates", get(handle_list_crates).post(handle_load_crate))
        .route("/api/crates/{crate_spec}", get(handle_crate_status))
        // MCP SSE proxy — routes /mcp/{crate}/sse and /mcp/{crate}/message
        // to per-crate SSE servers, transparent to the client
        .route("/mcp/{crate_spec}/sse", get(proxy_sse))
        .route("/mcp/{crate_spec}/message", post(proxy_message))
        .with_state(registry.clone());

    let addr: SocketAddr = format!("{}:{}", host, management_port)
        .parse()
        .map_err(|e| ServerError::Config(format!("Invalid management address: {}", e)))?;

    eprintln!("🦀 Rust Docs MCP Cloud Service v{}", env!("CARGO_PKG_VERSION"));
    eprintln!("   Management API:  http://{}/health", addr);
    eprintln!("   Load a crate:    POST http://{}/api/crates", addr);
    eprintln!(
        "   Pre-loaded:      {} crates",
        registry.instances.read().await.len()
    );

    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        ServerError::Config(format!("Failed to bind management port {}: {}", management_port, e))
    })?;

    axum::serve(listener, app).await.map_err(|e| {
        ServerError::Config(format!("Management server error: {}", e))
    })?;

    Ok(())
}

//! 원격 커넥터용 OAuth authorization-code + PKCE와 Streamable HTTP.
//! 계정·토큰은 메모리에만 보관하며 서버 재시작 시 다시 연결해야 합니다.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Form, Json, Router,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{any, get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rmcp::transport::{
    StreamableHttpServerConfig, StreamableHttpService,
    streamable_http_server::session::local::LocalSessionManager,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tower::ServiceExt;
use url::Url;

use crate::server::SsuServer;

const CALLBACK: &str = "https://claude.ai/api/mcp/auth_callback";
const SMARTID_LOGIN: &str = "https://smartid.ssu.ac.kr/Symtra_sso/smln.asp";

/// Claude 웹/앱의 고정 콜백, 또는 Claude Code·Codex 같은 CLI 가 띄우는 루프백 콜백(RFC 8252)만 허용합니다.
/// 루프백은 로그인한 사람 자신의 컴퓨터로만 돌아가므로 남이 등록해도 코드를 가로챌 수 없습니다.
fn redirect_allowed(uri: &str) -> bool {
    if uri == CALLBACK {
        return true;
    }
    let Ok(url) = Url::parse(uri) else {
        return false;
    };
    url.scheme() == "http"
        && matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}
const TTL: Duration = Duration::from_secs(300);
/// 접속 토큰은 짧게 두고, 갱신 토큰으로 Claude 가 알아서 새로 받습니다.
const TOKEN_TTL: Duration = Duration::from_secs(3600);
/// 갱신 토큰은 쓸 때마다 새것으로 바뀝니다(rotation). 30일 동안 쓰지 않으면 다시 연결해야 합니다.
const REFRESH_TTL: Duration = Duration::from_secs(30 * 86400);

fn secret() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

struct Expiring<T> {
    value: T,
    until: Instant,
}
impl<T> Expiring<T> {
    fn new(value: T, ttl: Duration) -> Self {
        Self {
            value,
            until: Instant::now() + ttl,
        }
    }
    fn alive(&self) -> bool {
        Instant::now() < self.until
    }
}

#[derive(Clone)]
struct App {
    base: String,
    /// 로그인을 허용할 학번. 목록에 없으면 SmartID 에 보내지 않습니다.
    allowed: Arc<Vec<String>>,
    /// 학교 SmartID 로 로그인하면 목록에 없는 학번도 받습니다(`RUSAINT_SSO_OPEN=1`).
    /// 비밀번호를 서버에 보내지 않으므로 남의 계정을 잠글 수 없습니다. 비밀번호 로그인은 계속 목록만 허용합니다.
    sso_open: bool,
    data: Arc<Mutex<Data>>,
}
#[derive(Default)]
struct Data {
    /// 클라이언트별로 등록한 콜백
    clients: HashMap<String, Expiring<Vec<String>>>,
    logins: HashMap<String, Expiring<Authorization>>,
    codes: HashMap<String, Expiring<(Authorization, SsuServer)>>,
    /// SmartID 로그인은 끝났고 도서관 계정(선택)을 기다리는 연결
    sso_pending: HashMap<String, Expiring<(Authorization, Arc<rusaint_sso::SsoCredentials>)>>,
    /// SmartID 가 같은 콜백을 두 번 보낼 때 두 번째 요청에도 같은 다음 화면을 보여 주려고 쓴 티켓의 결과를 잠시 둡니다.
    sso_used: HashMap<String, Expiring<SsoUsed>>,
    /// 도서관 단계까지 끝났고 에브리타임 계정(선택)을 기다리는 연결
    #[allow(clippy::type_complexity)]
    library_pending: HashMap<
        String,
        Expiring<(
            Authorization,
            Arc<rusaint_sso::SsoCredentials>,
            Option<Arc<rusaint_library::LibraryClient>>,
        )>,
    >,
    tokens: HashMap<String, Expiring<SsuServer>>,
    refreshes: HashMap<String, Expiring<Refresh>>,
    attempts: Vec<Instant>,
}

/// 이미 쓴 SmartID 콜백 티켓의 처리 결과
#[derive(Clone)]
enum SsoUsed {
    /// 첫 요청이 아직 학교 토큰을 확인하는 중
    Checking,
    /// 성공해서 이 티켓으로 도서관 단계를 이어감
    Next(String),
    Failed,
}

/// 갱신 토큰이 묶인 클라이언트·리소스와 로그인된 서버
struct Refresh {
    client_id: String,
    resource: String,
    server: SsuServer,
}
impl Data {
    fn prune(&mut self) {
        self.clients.retain(|_, v| v.alive());
        self.logins.retain(|_, v| v.alive());
        self.codes.retain(|_, v| v.alive());
        self.sso_pending.retain(|_, v| v.alive());
        self.sso_used.retain(|_, v| v.alive());
        self.library_pending.retain(|_, v| v.alive());
        self.tokens.retain(|_, v| v.alive());
        self.refreshes.retain(|_, v| v.alive());
        self.attempts
            .retain(|v| v.elapsed() < Duration::from_secs(60));
    }
}

type HttpError = (StatusCode, Json<Value>);
fn error(status: StatusCode, code: &str) -> HttpError {
    (status, Json(json!({"error": code})))
}
fn invalid() -> HttpError {
    error(StatusCode::BAD_REQUEST, "invalid_request")
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::var("RUSAINT_MCP_PUBLIC_URL")?;
    let url = Url::parse(&base)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err("RUSAINT_MCP_PUBLIC_URL 은 경로 없는 공개 HTTPS 주소여야 합니다".into());
    }
    let allowed = allowed_ids(&std::env::var("RUSAINT_ALLOWED_IDS").unwrap_or_default());
    let sso_open = std::env::var("RUSAINT_SSO_OPEN").is_ok_and(|v| v == "1");
    if allowed.is_empty() && !sso_open {
        return Err("RUSAINT_ALLOWED_IDS 에 로그인을 허용할 학번을 쉼표로 넣거나, 학교 SSO 로그인을 누구나 쓰게 하려면 RUSAINT_SSO_OPEN=1 을 설정하세요".into());
    }
    if sso_open {
        eprintln!("학교 SSO 로그인: 모든 학번 허용 (비밀번호 로그인은 RUSAINT_ALLOWED_IDS 만)");
    }
    let app = App {
        base: base.trim_end_matches('/').to_string(),
        allowed: Arc::new(allowed),
        sso_open,
        data: Arc::default(),
    };
    let cleanup = app.data.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            cleanup.lock().await.prune();
        }
    });
    let router = router(app);
    let addr = std::env::var("RUSAINT_MCP_BIND").unwrap_or_else(|_| "127.0.0.1:3000".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("원격 MCP: {addr} (HTTPS reverse proxy 필요)");
    axum::serve(listener, router).await?;
    Ok(())
}

fn allowed_ids(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect()
}

fn router(app: App) -> Router {
    Router::new()
        .route("/.well-known/oauth-protected-resource", get(resource))
        .route("/.well-known/oauth-protected-resource/mcp", get(resource))
        .route("/.well-known/oauth-authorization-server", get(metadata))
        .route("/register", post(register))
        .route("/authorize", get(authorize))
        .route("/login", post(login))
        .route("/sso/callback/{ticket}", get(sso_callback))
        .route("/sso/finish", post(sso_finish))
        .route("/sso/everytime", post(sso_everytime))
        .route("/token", post(token))
        .route("/mcp", any(mcp))
        .layer(axum::middleware::map_response(
            |mut response: Response| async move {
                let headers = response.headers_mut();
                headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
                headers.insert("referrer-policy", "no-referrer".parse().unwrap());
                headers.insert("x-content-type-options", "nosniff".parse().unwrap());
                headers.insert("content-security-policy", crate::page::CSP.parse().unwrap());
                response
            },
        ))
        // 쿼리에는 code·state·sToken 이 들어 있어 경로와 상태 코드만 남깁니다.
        .layer(axum::middleware::from_fn(
            |request: Request, next: axum::middleware::Next| async move {
                let (method, mut path) =
                    (request.method().clone(), request.uri().path().to_owned());
                // 경로의 1회용 티켓은 남기지 않습니다.
                if path.starts_with("/sso/callback/") {
                    path = "/sso/callback/…".into();
                }
                let response = next.run(request).await;
                eprintln!("{method} {path} -> {}", response.status().as_u16());
                response
            },
        ))
        .with_state(app)
}

async fn resource(State(app): State<App>) -> Json<Value> {
    Json(
        json!({"resource": format!("{}/mcp", app.base), "authorization_servers": [app.base], "scopes_supported": ["ssu"], "bearer_methods_supported": ["header"]}),
    )
}
async fn metadata(State(app): State<App>) -> Json<Value> {
    Json(
        json!({"issuer": app.base, "authorization_endpoint": format!("{}/authorize", app.base), "token_endpoint": format!("{}/token", app.base), "registration_endpoint": format!("{}/register", app.base), "response_types_supported": ["code"], "grant_types_supported": ["authorization_code", "refresh_token"], "token_endpoint_auth_methods_supported": ["none"], "code_challenge_methods_supported": ["S256"], "scopes_supported": ["ssu"]}),
    )
}

#[derive(Deserialize)]
struct Registration {
    redirect_uris: Vec<String>,
    token_endpoint_auth_method: Option<String>,
}
async fn register(
    State(app): State<App>,
    Json(req): Json<Registration>,
) -> Result<impl IntoResponse, HttpError> {
    // 허용한 콜백만 받아 공개 등록을 통한 피싱/리디렉션을 차단합니다.
    if req.redirect_uris.is_empty()
        || req.redirect_uris.len() > 4
        || !req.redirect_uris.iter().all(|uri| redirect_allowed(uri))
        || req
            .token_endpoint_auth_method
            .as_deref()
            .is_some_and(|v| v != "none")
    {
        eprintln!(
            "등록 거부: redirect_uris={:?} token_endpoint_auth_method={:?}",
            req.redirect_uris, req.token_endpoint_auth_method
        );
        return Err(invalid());
    }
    let mut data = app.data.lock().await;
    data.prune();
    if data.clients.len() >= 256 {
        return Err(error(
            StatusCode::TOO_MANY_REQUESTS,
            "temporarily_unavailable",
        ));
    }
    let id = secret();
    data.clients.insert(
        id.clone(),
        Expiring::new(req.redirect_uris.clone(), Duration::from_secs(30 * 86400)),
    );
    Ok((
        StatusCode::CREATED,
        Json(
            json!({"client_id": id, "redirect_uris": req.redirect_uris, "token_endpoint_auth_method": "none", "grant_types": ["authorization_code", "refresh_token"], "response_types": ["code"]}),
        ),
    ))
}

#[derive(Clone, Deserialize)]
struct Authorization {
    client_id: String,
    redirect_uri: String,
    response_type: String,
    code_challenge: String,
    code_challenge_method: String,
    state: Option<String>,
    resource: String,
    scope: Option<String>,
}
async fn authorize(
    State(app): State<App>,
    Query(req): Query<Authorization>,
) -> Result<Html<String>, HttpError> {
    let mut data = app.data.lock().await;
    data.prune();
    // 어느 검사에서 막혔는지만 남깁니다(code_challenge·state 값은 남기지 않음).
    let failed = if !data.clients.contains_key(&req.client_id) {
        Some("등록되지 않은 client_id")
    } else if !data
        .clients
        .get(&req.client_id)
        .is_some_and(|c| c.value.contains(&req.redirect_uri))
    {
        Some("등록하지 않은 redirect_uri")
    } else if req.response_type != "code" {
        Some("response_type")
    } else if req.code_challenge_method != "S256"
        || URL_SAFE_NO_PAD
            .decode(&req.code_challenge)
            .map_or(true, |v| v.len() != 32)
    {
        Some("PKCE")
    } else if req.resource != format!("{}/mcp", app.base) {
        Some("resource")
    } else if req.scope.as_deref().is_some_and(|v| v != "ssu") {
        Some("scope")
    } else if req.state.as_ref().is_some_and(|v| v.len() > 2048) {
        Some("state 길이")
    } else {
        None
    };
    if let Some(reason) = failed {
        eprintln!(
            "authorize 거부: {reason} (redirect_uri={:?} resource={:?} scope={:?})",
            req.redirect_uri, req.resource, req.scope
        );
        return Err(invalid());
    }
    if data.logins.len() >= 256 {
        return Err(error(
            StatusCode::TOO_MANY_REQUESTS,
            "temporarily_unavailable",
        ));
    }
    let ticket = secret();
    data.logins.insert(ticket.clone(), Expiring::new(req, TTL));
    let sso_url = smartid_url(&app, &ticket);
    Ok(Html(crate::page::login(&ticket, &sso_url)))
}

#[derive(Deserialize)]
struct Login {
    ticket: String,
    id: String,
    password: String,
    library_id: String,
    library_password: String,
}
async fn login(State(app): State<App>, Form(req): Form<Login>) -> Result<Response, HttpError> {
    let auth = {
        let mut data = app.data.lock().await;
        data.prune();
        if data.attempts.len() >= 10 || data.refreshes.len() + data.codes.len() >= 128 {
            return Err(error(
                StatusCode::TOO_MANY_REQUESTS,
                "temporarily_unavailable",
            ));
        }
        let auth = data.logins.remove(&req.ticket).ok_or_else(invalid)?.value;
        data.attempts.push(Instant::now());
        auth
    };
    if req.id.trim().is_empty()
        || req.id.len() > 128
        || req.password.is_empty()
        || req.password.len() > 1024
        || req.library_id.len() > 128
        || req.library_password.len() > 1024
        || req.library_id.is_empty() != req.library_password.is_empty()
    {
        return Err(invalid());
    }
    let id = req.id.trim().to_string();
    // 다른 사람 계정이 이 서버에 맡겨지거나, 남의 학번으로 틀린 비밀번호를 보내 잠그는 데 쓰이지 않도록
    // 허용 목록에 없는 학번은 SmartID 에 보내지 않습니다.
    if !app.allowed.contains(&id) {
        return Ok((
            StatusCode::FORBIDDEN,
            Html(crate::page::message(
                "운영자에게 등록을 요청해 주세요",
                "비밀번호 연결은 등록한 학번만 쓸 수 있어요. 학교 로그인으로 연결하면 등록 없이 쓸 수 있어요. 학교 서버에는 아무것도 보내지 않았어요.",
            )),
        )
            .into_response());
    }
    let sso = rusaint_sso::SsoCredentials::new(id, req.password);
    let verified = async {
        sso.token().await.map_err(|_| ())?;
        let library = if req.library_id.is_empty() {
            None
        } else {
            let client = Arc::new(
                rusaint_library::LibraryClient::new(req.library_id, req.library_password)
                    .map_err(|_| ())?,
            );
            client.login().await.map_err(|_| ())?;
            Some(client)
        };
        SsuServer::authenticated(sso, library, None).map_err(|_| ())
    };
    let server = match tokio::time::timeout(Duration::from_secs(60), verified).await {
        Ok(Ok(server)) => server,
        _ => return Ok((
            StatusCode::UNAUTHORIZED,
            Html(crate::page::message(
                "학번이나 비밀번호를 확인해 주세요",
                "Claude에서 연결을 다시 눌러 주세요. 여러 번 틀리면 학교 계정이 잠길 수 있어요.",
            )),
        )
            .into_response()),
    };
    issue_code(&app, auth, server).await
}

/// SmartID 로그인 화면 주소. 로그인이 끝나면 SmartID 가 `apiReturnUrl` 뒤에 `?sToken=…&sIdno=학번` 을 붙여 돌려보냅니다.
fn smartid_url(app: &App, ticket: &str) -> String {
    let callback = format!("{}/sso/callback/{ticket}", app.base);
    let encoded: String = url::form_urlencoded::byte_serialize(callback.as_bytes()).collect();
    format!("{SMARTID_LOGIN}?apiReturnUrl={encoded}")
}

#[derive(Deserialize)]
struct SsoReturn {
    #[serde(rename = "sToken")]
    token: Option<String>,
    #[serde(rename = "sIdno")]
    id: Option<String>,
}

fn page(status: StatusCode, title: &str, detail: &str) -> Response {
    (status, Html(crate::page::message(title, detail))).into_response()
}

/// SmartID 에서 돌아오는 곳. 비밀번호는 학교 화면에만 입력되고, 이 서버는 학교가 발급한 토큰만 받습니다.
async fn sso_callback(
    State(app): State<App>,
    Path(ticket): Path<String>,
    Query(ret): Query<SsoReturn>,
) -> Result<Response, HttpError> {
    // SmartID 는 돌아오는 요청을 두 번 보내기도 합니다. 이미 쓴 티켓이면 첫 요청의 결과 화면을 다시 보여 줍니다.
    if let Some(response) = repeated_callback(&app, &ticket).await {
        return Ok(response);
    }
    // 토큰이 없는 요청은 티켓을 쓰지 않고 무시합니다.
    let (Some(token), Some(id)) = (
        ret.token.filter(|t| !t.is_empty() && t.len() <= 4096),
        ret.id.map(|i| i.trim().to_string()),
    ) else {
        return Ok(page(
            StatusCode::BAD_REQUEST,
            "학교 로그인을 다시 해 주세요",
            "학교에서 로그인 정보를 받지 못했어요. Claude에서 연결을 다시 눌러 주세요.",
        ));
    };
    let auth = {
        let mut data = app.data.lock().await;
        data.prune();
        if data.attempts.len() >= 10 || data.sso_pending.len() >= 64 {
            return Err(error(
                StatusCode::TOO_MANY_REQUESTS,
                "temporarily_unavailable",
            ));
        }
        let Some(auth) = data.logins.remove(&ticket) else {
            return Ok(page(
                StatusCode::BAD_REQUEST,
                "처음부터 다시 연결해 주세요",
                "보안을 위해 로그인 화면은 5분만 열려 있어요. Claude에서 연결을 다시 눌러 주세요.",
            ));
        };
        data.attempts.push(Instant::now());
        data.sso_used
            .insert(ticket.clone(), Expiring::new(SsoUsed::Checking, TTL));
        auth.value
    };
    let id_shape =
        !id.is_empty() && id.len() <= 20 && id.chars().all(|c| c.is_ascii_alphanumeric());
    if !id_shape || !(app.sso_open || app.allowed.contains(&id)) {
        mark_used(&app, &ticket, SsoUsed::Failed).await;
        return Ok(page(
            StatusCode::FORBIDDEN,
            "운영자에게 등록을 요청해 주세요",
            "이 서버는 등록한 학번만 쓸 수 있어요. 받은 로그인 정보는 바로 지웠어요.",
        ));
    }
    // 토큰이 진짜 학교 토큰이고 그 학번의 것인지 u-saint 세션을 실제로 열어 확인합니다.
    let verified = tokio::time::timeout(
        Duration::from_secs(30),
        rusaint::USaintSession::with_token(&id, &token),
    )
    .await;
    if !matches!(verified, Ok(Ok(_))) {
        mark_used(&app, &ticket, SsoUsed::Failed).await;
        return Ok(page(
            StatusCode::UNAUTHORIZED,
            "학교 로그인을 다시 해 주세요",
            "학교 로그인이 확인되지 않았어요. Claude에서 연결을 다시 눌러 주세요.",
        ));
    }
    let sso = rusaint_sso::SsoCredentials::from_token(id.clone(), token);
    let next = secret();
    {
        let mut data = app.data.lock().await;
        data.sso_pending
            .insert(next.clone(), Expiring::new((auth, sso), TTL));
        data.sso_used
            .insert(ticket, Expiring::new(SsoUsed::Next(next.clone()), TTL));
    }
    Ok(Html(crate::page::library_step(&next, false)).into_response())
}

async fn mark_used(app: &App, ticket: &str, state: SsoUsed) {
    app.data
        .lock()
        .await
        .sso_used
        .insert(ticket.to_string(), Expiring::new(state, TTL));
}

/// 이미 쓴 티켓으로 다시 들어온 콜백이면 첫 요청이 끝나기를 기다려 같은 결과를 돌려줍니다.
async fn repeated_callback(app: &App, ticket: &str) -> Option<Response> {
    let deadline = Instant::now() + Duration::from_secs(35);
    loop {
        let state = {
            let mut data = app.data.lock().await;
            data.prune();
            match data.sso_used.get(ticket).map(|v| v.value.clone())? {
                SsoUsed::Next(next) if data.sso_pending.contains_key(&next) => {
                    return Some(Html(crate::page::library_step(&next, false)).into_response());
                }
                other => other,
            }
        };
        match state {
            SsoUsed::Checking if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            SsoUsed::Next(_) => {
                return Some(page(
                    StatusCode::OK,
                    "다른 화면에서 이어서 연결하고 있어요",
                    "학교 로그인은 끝났어요. 먼저 열린 화면에서 계속해 주세요.",
                ));
            }
            _ => {
                return Some(page(
                    StatusCode::UNAUTHORIZED,
                    "학교 로그인을 다시 해 주세요",
                    "학교 로그인이 확인되지 않았어요. Claude에서 연결을 다시 눌러 주세요.",
                ));
            }
        }
    }
}

#[derive(Deserialize)]
struct Finish {
    ticket: String,
    /// "건너뛰기" 버튼. 입력한 도서관 계정은 무시합니다.
    #[serde(default)]
    skip: Option<String>,
    #[serde(default)]
    library_id: String,
    #[serde(default)]
    library_password: String,
}

/// SSO 다음 단계: 도서관 계정(선택)을 받고 연결을 마칩니다. 도서관은 SSO 가 없어 비밀번호를 받아야 합니다.
async fn sso_finish(
    State(app): State<App>,
    Form(req): Form<Finish>,
) -> Result<Response, HttpError> {
    let (auth, sso) = {
        let mut data = app.data.lock().await;
        data.prune();
        if data.attempts.len() >= 10 || data.refreshes.len() + data.codes.len() >= 128 {
            return Err(error(
                StatusCode::TOO_MANY_REQUESTS,
                "temporarily_unavailable",
            ));
        }
        data.sso_pending
            .remove(&req.ticket)
            .ok_or_else(invalid)?
            .value
    };
    let (library_id, library_password) = if req.skip.is_some() {
        (String::new(), String::new())
    } else {
        (req.library_id, req.library_password)
    };
    if library_id.len() > 128
        || library_password.len() > 1024
        || library_id.trim().is_empty() != library_password.is_empty()
    {
        return Err(invalid());
    }
    let library = if library_id.trim().is_empty() {
        None
    } else {
        app.data.lock().await.attempts.push(Instant::now());
        let login = async {
            let client = Arc::new(
                rusaint_library::LibraryClient::new(library_id.trim(), library_password)
                    .map_err(|_| ())?,
            );
            client.login().await.map_err(|_| ())?;
            Ok::<_, ()>(client)
        };
        match tokio::time::timeout(Duration::from_secs(60), login).await {
            Ok(Ok(client)) => Some(client),
            _ => {
                // 통합 로그인은 이미 끝났으니 도서관만 다시 입력하거나 건너뛸 수 있게 합니다.
                let next = secret();
                app.data
                    .lock()
                    .await
                    .sso_pending
                    .insert(next.clone(), Expiring::new((auth, sso), TTL));
                return Ok((
                    StatusCode::UNAUTHORIZED,
                    Html(crate::page::library_step(&next, true)),
                )
                    .into_response());
            }
        }
    };
    // 도서관 단계가 끝났으니 에브리타임(선택) 단계로 넘어갑니다.
    let next = secret();
    app.data
        .lock()
        .await
        .library_pending
        .insert(next.clone(), Expiring::new((auth, sso, library), TTL));
    Ok(Html(crate::page::everytime_step(&next, false)).into_response())
}

#[derive(Deserialize)]
struct EverytimeFinish {
    ticket: String,
    /// "건너뛰기" 버튼. 입력한 에브리타임 계정은 무시합니다.
    #[serde(default)]
    skip: Option<String>,
    /// 브라우저에서 복사한 세션 쿠키(etsid)
    #[serde(default)]
    everytime_cookie: String,
}

/// 마지막 단계: 에브리타임 세션 쿠키(선택)를 받고 연결을 마칩니다. 에타 로그인은 캡차·앱 승인이 있어 사용자가 브라우저에서 한 뒤 쿠키만 넘깁니다.
async fn sso_everytime(
    State(app): State<App>,
    Form(req): Form<EverytimeFinish>,
) -> Result<Response, HttpError> {
    let (auth, sso, library) = {
        let mut data = app.data.lock().await;
        data.prune();
        if data.attempts.len() >= 10 || data.refreshes.len() + data.codes.len() >= 128 {
            return Err(error(
                StatusCode::TOO_MANY_REQUESTS,
                "temporarily_unavailable",
            ));
        }
        data.library_pending
            .remove(&req.ticket)
            .ok_or_else(invalid)?
            .value
    };
    let cookie = if req.skip.is_some() {
        String::new()
    } else {
        req.everytime_cookie
    };
    if cookie.len() > 4096 {
        return Err(invalid());
    }
    let everytime = if cookie.trim().is_empty() {
        None
    } else {
        app.data.lock().await.attempts.push(Instant::now());
        let login = async {
            let client = Arc::new(
                rusaint_everytime::EverytimeClient::from_session(&cookie).map_err(|_| ())?,
            );
            client.verify().await.map_err(|_| ())?;
            Ok::<_, ()>(client)
        };
        match tokio::time::timeout(Duration::from_secs(30), login).await {
            Ok(Ok(client)) => Some(client),
            _ => {
                // 앞 단계는 모두 끝났으니 에타만 다시 입력하거나 건너뛸 수 있게 합니다.
                let next = secret();
                app.data
                    .lock()
                    .await
                    .library_pending
                    .insert(next.clone(), Expiring::new((auth, sso, library), TTL));
                return Ok((
                    StatusCode::UNAUTHORIZED,
                    Html(crate::page::everytime_step(&next, true)),
                )
                    .into_response());
            }
        }
    };
    let server = SsuServer::authenticated(sso, library, everytime).map_err(|_| invalid())?;
    server.spawn_keepalive();
    issue_code(&app, auth, server).await
}

/// 로그인을 마친 서버를 인증 코드에 묶고 앱의 콜백으로 돌려보냅니다.
async fn issue_code(
    app: &App,
    auth: Authorization,
    server: SsuServer,
) -> Result<Response, HttpError> {
    let code = secret();
    let mut redirect = Url::parse(&auth.redirect_uri).map_err(|_| invalid())?;
    redirect
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("iss", &app.base);
    if let Some(state) = &auth.state {
        redirect.query_pairs_mut().append_pair("state", state);
    }
    app.data
        .lock()
        .await
        .codes
        .insert(code, Expiring::new((auth, server), TTL));
    Ok(Redirect::to(redirect.as_str()).into_response())
}

#[derive(Deserialize)]
struct Token {
    grant_type: String,
    client_id: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    resource: Option<String>,
}
async fn token(State(app): State<App>, Form(req): Form<Token>) -> Result<Json<Value>, HttpError> {
    let grant = || error(StatusCode::BAD_REQUEST, "invalid_grant");
    let mut data = app.data.lock().await;
    data.prune();
    match req.grant_type.as_str() {
        "authorization_code" => {
            let (auth, server) = data
                .codes
                .remove(req.code.as_deref().ok_or_else(grant)?)
                .ok_or_else(grant)?
                .value;
            let verifier = req.code_verifier.as_deref().unwrap_or_default();
            if auth.client_id != req.client_id
                || Some(&auth.redirect_uri) != req.redirect_uri.as_ref()
                || Some(&auth.resource) != req.resource.as_ref()
                || !(43..=128).contains(&verifier.len())
                || !verifier
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
                || URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
                    != auth.code_challenge
            {
                return Err(grant());
            }
            Ok(issue(&mut data, auth.client_id, auth.resource, server))
        }
        "refresh_token" => {
            // 갱신 토큰은 한 번 쓰면 사라지고 새것을 줍니다. 묶인 클라이언트·리소스가 다르면 거부합니다.
            let refresh = data
                .refreshes
                .remove(req.refresh_token.as_deref().ok_or_else(grant)?)
                .ok_or_else(grant)?
                .value;
            if refresh.client_id != req.client_id
                || req
                    .resource
                    .as_ref()
                    .is_some_and(|r| r != &refresh.resource)
            {
                return Err(grant());
            }
            Ok(issue(
                &mut data,
                refresh.client_id,
                refresh.resource,
                refresh.server,
            ))
        }
        _ => Err(error(StatusCode::BAD_REQUEST, "unsupported_grant_type")),
    }
}

/// 접속 토큰과 갱신 토큰을 새로 발급합니다.
fn issue(data: &mut Data, client_id: String, resource: String, server: SsuServer) -> Json<Value> {
    let access = secret();
    let refresh = secret();
    data.tokens
        .insert(access.clone(), Expiring::new(server.clone(), TOKEN_TTL));
    data.refreshes.insert(
        refresh.clone(),
        Expiring::new(
            Refresh {
                client_id,
                resource,
                server,
            },
            REFRESH_TTL,
        ),
    );
    Json(json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": TOKEN_TTL.as_secs(),
        "refresh_token": refresh,
        "scope": "ssu",
    }))
}

async fn mcp(State(app): State<App>, request: Request) -> Response {
    let server = {
        let mut data = app.data.lock().await;
        data.prune();
        request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .and_then(|t| data.tokens.get(t))
            .map(|v| v.value.clone())
    };
    let Some(server) = server else {
        return (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                format!(
                    "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
                    app.base
                ),
            )],
        )
            .into_response();
    };
    let host = Url::parse(&app.base)
        .unwrap()
        .host_str()
        .unwrap()
        .to_string();
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_allowed_hosts(vec![host, "localhost".into(), "127.0.0.1".into()]);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    match service.oneshot(request).await {
        Ok(response) => response.into_response(),
        Err(never) => match never {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};

    fn app() -> App {
        App {
            base: "https://mcp.example.com".into(),
            allowed: Arc::new(allowed_ids(" 20240000 , ,20249999")),
            sso_open: false,
            data: Arc::default(),
        }
    }

    #[test]
    fn parses_allowed_ids() {
        assert_eq!(
            allowed_ids(" 20240000 , ,20249999"),
            ["20240000", "20249999"]
        );
        assert!(allowed_ids(" , ").is_empty());
    }

    #[tokio::test]
    async fn csp_allows_redirect_back_to_claude() {
        let response = router(app())
            .oneshot(Request::builder().uri("/mcp").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let csp = response.headers()["content-security-policy"]
            .to_str()
            .unwrap();
        assert!(
            csp.contains("form-action 'self' https://claude.ai"),
            "{csp}"
        );
    }

    #[tokio::test]
    async fn rejects_ids_outside_allowlist_before_smartid() {
        let app = app();
        app.data
            .lock()
            .await
            .logins
            .insert("ticket".into(), Expiring::new(auth(), TTL));
        let response = login(
            State(app.clone()),
            Form(Login {
                ticket: "ticket".into(),
                id: " 19990000 ".into(),
                password: "x".into(),
                library_id: "".into(),
                library_password: "".into(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        // 거부돼도 티켓은 다시 쓸 수 없다
        assert!(app.data.lock().await.logins.is_empty());
    }
    #[test]
    fn smartid_returns_to_the_ticket_callback() {
        let url = smartid_url(&app(), "abc123");
        assert_eq!(
            url,
            "https://smartid.ssu.ac.kr/Symtra_sso/smln.asp?apiReturnUrl=https%3A%2F%2Fmcp.example.com%2Fsso%2Fcallback%2Fabc123"
        );
    }

    async fn callback(app: &App, ticket: &str, token: Option<&str>, id: Option<&str>) -> Response {
        sso_callback(
            State(app.clone()),
            Path(ticket.into()),
            Query(SsoReturn {
                token: token.map(Into::into),
                id: id.map(Into::into),
            }),
        )
        .await
        .unwrap()
    }

    #[test]
    #[ignore = "RUSAINT_PAGE_PREVIEW=<dir> 로 미리보기 HTML 을 만듭니다"]
    fn write_page_previews() {
        let dir = std::path::PathBuf::from(std::env::var("RUSAINT_PAGE_PREVIEW").unwrap());
        let pages = [
            (
                "1-login.html",
                crate::page::login("t", &smartid_url(&app(), "t")),
            ),
            ("2-library.html", crate::page::library_step("t", false)),
            ("3-library-error.html", crate::page::library_step("t", true)),
            ("4-everytime.html", crate::page::everytime_step("t", false)),
            (
                "5-everytime-error.html",
                crate::page::everytime_step("t", true),
            ),
            (
                "6-expired.html",
                crate::page::message(
                    "처음부터 다시 연결해 주세요",
                    "보안을 위해 로그인 화면은 5분만 열려 있어요. Claude에서 연결을 다시 눌러 주세요.",
                ),
            ),
        ];
        for (name, html) in pages {
            std::fs::write(dir.join(name), html).unwrap();
        }
    }

    #[tokio::test]
    async fn sso_callback_needs_a_live_ticket_and_an_allowed_id() {
        let app = app();
        // 모르는 티켓
        let r = callback(&app, "nope", Some("t"), Some("20240000")).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);

        // 허용 목록 밖 학번: u-saint 로 확인하지도 않고 거부, 티켓은 소모
        app.data
            .lock()
            .await
            .logins
            .insert("ticket".into(), Expiring::new(auth(), TTL));
        let r = callback(&app, "ticket", Some("t"), Some("19990000")).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert!(app.data.lock().await.logins.is_empty());
        assert!(app.data.lock().await.sso_pending.is_empty());

        // 거부된 티켓으로 다시 오면 같은 실패 화면
        let r = callback(&app, "ticket", Some("t"), Some("19990000")).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

        // SmartID 가 토큰 없이 돌아온 경우
        app.data
            .lock()
            .await
            .logins
            .insert("fresh".into(), Expiring::new(auth(), TTL));
        let r = callback(&app, "fresh", None, Some("20240000")).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        // 토큰 없는 중복 요청은 티켓을 쓰지 않는다
        assert!(app.data.lock().await.logins.contains_key("fresh"));
    }

    #[tokio::test]
    async fn repeated_sso_callback_shows_the_same_next_step() {
        let app = app();
        {
            let mut data = app.data.lock().await;
            data.sso_pending.insert(
                "next".into(),
                Expiring::new(
                    (
                        auth(),
                        rusaint_sso::SsoCredentials::from_token("20240000", "tok"),
                    ),
                    TTL,
                ),
            );
            data.sso_used.insert(
                "used".into(),
                Expiring::new(SsoUsed::Next("next".into()), TTL),
            );
        }
        // 토큰이 있든 없든 같은 도서관 단계(같은 다음 티켓)를 보여 준다
        for token in [Some("t"), None] {
            let r = callback(&app, "used", token, Some("20240000")).await;
            assert_eq!(r.status(), StatusCode::OK);
            let body = axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap();
            assert!(String::from_utf8_lossy(&body).contains(r#"value="next""#));
        }
        // 다음 단계로 이미 넘어갔으면 안내만 한다
        app.data.lock().await.sso_pending.clear();
        let r = callback(&app, "used", Some("t"), Some("20240000")).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert!(app.data.lock().await.logins.is_empty());
    }

    #[tokio::test]
    async fn sso_open_still_rejects_malformed_ids_and_keeps_password_login_closed() {
        let app = App {
            sso_open: true,
            ..app()
        };
        app.data
            .lock()
            .await
            .logins
            .insert("ticket".into(), Expiring::new(auth(), TTL));
        let r = callback(&app, "ticket", Some("t"), Some("../x")).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        // 비밀번호 로그인은 sso_open 이어도 목록 밖 학번을 SmartID 에 보내지 않는다
        app.data
            .lock()
            .await
            .logins
            .insert("ticket".into(), Expiring::new(auth(), TTL));
        let r = login(
            State(app.clone()),
            Form(Login {
                ticket: "ticket".into(),
                id: "19990000".into(),
                password: "x".into(),
                library_id: "".into(),
                library_password: "".into(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn sso_finish_moves_to_everytime_step() {
        let app = app();
        app.data.lock().await.sso_pending.insert(
            "next".into(),
            Expiring::new(
                (
                    auth(),
                    rusaint_sso::SsoCredentials::from_token("20240000", "tok"),
                ),
                TTL,
            ),
        );
        // 도서관을 건너뛰면 에브리타임 단계로 넘어간다(아직 코드 발급 전).
        let r = sso_finish(
            State(app.clone()),
            Form(Finish {
                ticket: "next".into(),
                skip: None,
                library_id: "".into(),
                library_password: "".into(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let ticket = {
            let data = app.data.lock().await;
            assert!(data.sso_pending.is_empty());
            assert!(data.codes.is_empty());
            assert_eq!(data.library_pending.len(), 1);
            data.library_pending.keys().next().unwrap().clone()
        };

        // 같은 sso_pending 티켓은 다시 쓸 수 없다
        let again = sso_finish(
            State(app.clone()),
            Form(Finish {
                ticket: "next".into(),
                skip: None,
                library_id: "".into(),
                library_password: "".into(),
            }),
        )
        .await;
        assert!(again.is_err());

        // 에브리타임도 건너뛰면 코드가 발급된다.
        let r = sso_everytime(
            State(app.clone()),
            Form(EverytimeFinish {
                ticket: ticket.clone(),
                skip: None,
                everytime_cookie: "".into(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        let location = r.headers()[header::LOCATION].to_str().unwrap();
        assert!(location.starts_with(CALLBACK), "{location}");
        assert!(location.contains("state=opaque-state"));
        let data = app.data.lock().await;
        assert!(data.library_pending.is_empty());
        assert_eq!(data.codes.len(), 1);
    }

    fn auth() -> Authorization {
        Authorization {
            client_id: "client".into(),
            redirect_uri: CALLBACK.into(),
            response_type: "code".into(),
            code_challenge: URL_SAFE_NO_PAD.encode(Sha256::digest("v".repeat(43).as_bytes())),
            code_challenge_method: "S256".into(),
            state: Some("opaque-state".into()),
            resource: "https://mcp.example.com/mcp".into(),
            scope: Some("ssu".into()),
        }
    }
    fn exchange(verifier: &str) -> Token {
        Token {
            grant_type: "authorization_code".into(),
            client_id: "client".into(),
            code: Some("code".into()),
            redirect_uri: Some(CALLBACK.into()),
            code_verifier: Some(verifier.into()),
            refresh_token: None,
            resource: Some("https://mcp.example.com/mcp".into()),
        }
    }
    fn refresh(token: &str, client_id: &str) -> Token {
        Token {
            grant_type: "refresh_token".into(),
            client_id: client_id.into(),
            code: None,
            redirect_uri: None,
            code_verifier: None,
            refresh_token: Some(token.into()),
            resource: Some("https://mcp.example.com/mcp".into()),
        }
    }

    #[tokio::test]
    async fn refresh_tokens_rotate_and_are_bound_to_client() {
        let app = app();
        seed(&app, auth()).await;
        let first = token(State(app.clone()), Form(exchange(&"v".repeat(43))))
            .await
            .unwrap()
            .0;
        let r1 = first["refresh_token"].as_str().unwrap().to_string();
        assert_eq!(first["expires_in"], 3600);

        // 다른 클라이언트는 쓸 수 없고, 시도하면 그 토큰은 사라진다
        assert!(
            token(State(app.clone()), Form(refresh(&r1, "other")))
                .await
                .is_err()
        );
        assert!(
            token(State(app.clone()), Form(refresh(&r1, "client")))
                .await
                .is_err()
        );

        seed(&app, auth()).await;
        let r2 = token(State(app.clone()), Form(exchange(&"v".repeat(43))))
            .await
            .unwrap()
            .0["refresh_token"]
            .as_str()
            .unwrap()
            .to_string();
        let renewed = token(State(app.clone()), Form(refresh(&r2, "client")))
            .await
            .unwrap()
            .0;
        let r3 = renewed["refresh_token"].as_str().unwrap().to_string();
        assert_ne!(r2, r3);
        assert!(renewed["access_token"].as_str().is_some());
        // 쓴 갱신 토큰은 다시 못 쓰고, 새 갱신 토큰은 쓸 수 있다
        assert!(
            token(State(app.clone()), Form(refresh(&r2, "client")))
                .await
                .is_err()
        );
        assert!(
            token(State(app.clone()), Form(refresh(&r3, "client")))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn expired_refresh_tokens_are_rejected() {
        let app = app();
        seed(&app, auth()).await;
        let r = token(State(app.clone()), Form(exchange(&"v".repeat(43))))
            .await
            .unwrap()
            .0["refresh_token"]
            .as_str()
            .unwrap()
            .to_string();
        app.data.lock().await.refreshes.get_mut(&r).unwrap().until = Instant::now();
        assert!(
            token(State(app.clone()), Form(refresh(&r, "client")))
                .await
                .is_err()
        );
    }
    async fn seed(app: &App, authorization: Authorization) {
        let server = SsuServer::authenticated(
            rusaint_sso::SsoCredentials::new("test", "unused"),
            None,
            None,
        )
        .unwrap();
        app.data
            .lock()
            .await
            .codes
            .insert("code".into(), Expiring::new((authorization, server), TTL));
    }

    #[tokio::test]
    async fn unauthenticated_requests_advertise_discovery() {
        let response = router(app())
            .oneshot(Request::builder().uri("/mcp").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            response.headers()[header::WWW_AUTHENTICATE]
                .to_str()
                .unwrap()
                .contains("/.well-known/oauth-protected-resource")
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn registration_restricts_callbacks_and_login_requires_one_time_ticket() {
        let app = app();
        assert!(
            register(
                State(app.clone()),
                Json(Registration {
                    redirect_uris: vec!["https://attacker.example/callback".into()],
                    token_endpoint_auth_method: None,
                })
            )
            .await
            .is_err()
        );
        let form = || {
            Form(Login {
                ticket: "ticket".into(),
                id: "".into(),
                password: "".into(),
                library_id: "".into(),
                library_password: "".into(),
            })
        };
        assert!(login(State(app.clone()), form()).await.is_err());
        app.data
            .lock()
            .await
            .logins
            .insert("ticket".into(), Expiring::new(auth(), TTL));
        assert!(login(State(app.clone()), form()).await.is_err());
        assert!(app.data.lock().await.logins.is_empty());
        assert!(login(State(app), form()).await.is_err());
    }

    #[test]
    fn only_claude_and_loopback_callbacks_are_allowed() {
        for ok in [
            CALLBACK,
            "http://localhost:53682/callback",
            "http://127.0.0.1:1455/callback",
        ] {
            assert!(redirect_allowed(ok), "{ok}");
        }
        for bad in [
            "https://attacker.example/callback",
            "http://localhost.attacker.example/callback",
            "https://localhost/callback",
            "http://[::1]:8080/cb",
            "http://user@127.0.0.1/callback",
            "http://127.0.0.1/cb#frag",
            "https://claude.ai/api/mcp/auth_callback/x",
        ] {
            assert!(!redirect_allowed(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn rejects_unregistered_clients_redirects_and_plain_pkce() {
        let app = app();
        assert!(authorize(State(app.clone()), Query(auth())).await.is_err());
        app.data
            .lock()
            .await
            .clients
            .insert("client".into(), Expiring::new(vec![CALLBACK.into()], TTL));
        let mut req = auth();
        req.redirect_uri = "https://attacker.example/callback".into();
        assert!(authorize(State(app.clone()), Query(req)).await.is_err());
        let mut req = auth();
        req.code_challenge_method = "plain".into();
        assert!(authorize(State(app.clone()), Query(req)).await.is_err());
        let html = authorize(State(app.clone()), Query(auth()))
            .await
            .unwrap()
            .0;
        assert!(html.contains("type=\"password\""));
        assert_eq!(app.data.lock().await.logins.len(), 1);
    }

    #[tokio::test]
    async fn codes_are_bound_to_client_resource_and_verifier_and_consumed() {
        let app = app();
        for bad_field in ["client", "resource", "verifier", "redirect"] {
            seed(&app, auth()).await;
            let mut req = exchange(&"v".repeat(43));
            match bad_field {
                "client" => req.client_id = "different".into(),
                "resource" => req.resource = Some("https://other.example/mcp".into()),
                "verifier" => req.code_verifier = Some("x".repeat(43)),
                _ => req.redirect_uri = Some("https://other.example/callback".into()),
            }
            assert!(token(State(app.clone()), Form(req)).await.is_err());
            assert!(app.data.lock().await.codes.is_empty());
        }
        seed(&app, auth()).await;
        let result = token(State(app.clone()), Form(exchange(&"v".repeat(43))))
            .await
            .unwrap()
            .0;
        assert_eq!(result["token_type"], "Bearer");
        assert!(
            token(State(app.clone()), Form(exchange(&"v".repeat(43))))
                .await
                .is_err()
        );
        assert_eq!(app.data.lock().await.tokens.len(), 1);
    }

    #[tokio::test]
    async fn expired_tokens_and_codes_are_rejected() {
        let app = app();
        seed(&app, auth()).await;
        app.data.lock().await.codes.get_mut("code").unwrap().until = Instant::now();
        assert!(
            token(State(app.clone()), Form(exchange(&"v".repeat(43))))
                .await
                .is_err()
        );
        seed(&app, auth()).await;
        let access = token(State(app.clone()), Form(exchange(&"v".repeat(43))))
            .await
            .unwrap()
            .0["access_token"]
            .as_str()
            .unwrap()
            .to_string();
        app.data.lock().await.tokens.get_mut(&access).unwrap().until = Instant::now();
        let response = router(app)
            .oneshot(
                Request::builder()
                    .uri("/mcp")
                    .header(header::AUTHORIZATION, format!("Bearer {access}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn authenticated_mcp_initializes_without_host_credentials() {
        let app = app();
        seed(&app, auth()).await;
        let access = token(State(app.clone()), Form(exchange(&"v".repeat(43))))
            .await
            .unwrap()
            .0["access_token"]
            .as_str()
            .unwrap()
            .to_string();
        let response = router(app).oneshot(Request::builder().method("POST").uri("/mcp")
            .header(header::HOST, "mcp.example.com").header(header::AUTHORIZATION, format!("Bearer {access}"))
            .header(header::CONTENT_TYPE, "application/json").header(header::ACCEPT, "application/json, text/event-stream")
            .body(Body::from(json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}).to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 65536).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("rusaint-mcp"));
    }
}

//! 원격 커넥터용 OAuth authorization-code + PKCE와 Streamable HTTP.
//! 계정·토큰은 메모리에만 보관하며 서버 재시작 시 다시 연결해야 합니다.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Form, Json, Router,
    extract::{Query, Request, State},
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
    data: Arc<Mutex<Data>>,
}
#[derive(Default)]
struct Data {
    /// 클라이언트별로 등록한 콜백
    clients: HashMap<String, Expiring<Vec<String>>>,
    logins: HashMap<String, Expiring<Authorization>>,
    codes: HashMap<String, Expiring<(Authorization, SsuServer)>>,
    tokens: HashMap<String, Expiring<SsuServer>>,
    refreshes: HashMap<String, Expiring<Refresh>>,
    attempts: Vec<Instant>,
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
    if allowed.is_empty() {
        return Err("RUSAINT_ALLOWED_IDS 에 로그인을 허용할 학번을 쉼표로 넣어 주세요".into());
    }
    let app = App {
        base: base.trim_end_matches('/').to_string(),
        allowed: Arc::new(allowed),
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
        // 쿼리에는 code·state 가 들어 있어 경로와 상태 코드만 남깁니다.
        .layer(axum::middleware::from_fn(
            |request: Request, next: axum::middleware::Next| async move {
                let (method, path) = (request.method().clone(), request.uri().path().to_owned());
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
    if !data
        .clients
        .get(&req.client_id)
        .is_some_and(|c| c.value.contains(&req.redirect_uri))
        || req.response_type != "code"
        || req.code_challenge_method != "S256"
        || URL_SAFE_NO_PAD
            .decode(&req.code_challenge)
            .map_or(true, |v| v.len() != 32)
        || req.resource != format!("{}/mcp", app.base)
        || req.scope.as_deref().is_some_and(|v| v != "ssu")
        || req.state.as_ref().is_some_and(|v| v.len() > 2048)
    {
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
    Ok(Html(crate::page::login(&ticket)))
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
                "허용되지 않은 학번",
                "이 서버에 로그인할 수 있도록 등록된 학번이 아닙니다. 학교 서버에는 아무것도 보내지 않았습니다.",
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
        SsuServer::authenticated(sso, library).map_err(|_| ())
    };
    let server = match tokio::time::timeout(Duration::from_secs(60), verified).await {
        Ok(Ok(server)) => server,
        _ => return Ok((StatusCode::UNAUTHORIZED, Html(crate::page::message("로그인 실패", "계정 정보를 확인한 뒤 앱에서 연결을 처음부터 다시 시작하세요. 여러 번 틀리면 학교 계정이 잠길 수 있습니다."))).into_response()),
    };
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
        let server =
            SsuServer::authenticated(rusaint_sso::SsoCredentials::new("test", "unused"), None)
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

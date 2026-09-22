//! Stateless Streamable HTTP (MCP 2025-11-25) for remote clients.
//!
//! One POST per call, JSON in and JSON out, no session. The legacy HTTP+SSE
//! transport is not served. The hosted endpoint requires a short-lived
//! sign-in token. An API key is only for a copy you run yourself.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;

use crate::inbound_auth::{inbound_api_key, Gate, GateDeny, HttpAuthMode};
use crate::oidc_resource::{OidcPolicy, OidcVerifier};
use crate::server::Server;

type McpService = StreamableHttpService<Server, LocalSessionManager>;

#[derive(Clone)]
struct OauthState {
    policy: OidcPolicy,
    verifier: Option<Arc<OidcVerifier>>,
}

#[derive(Clone)]
struct App {
    gate: Gate,
    service: McpService,
    oauth: Option<OauthState>,
}

impl App {
    fn new(gate: Gate, cancel: CancellationToken, oauth: Option<OauthState>) -> Self {
        let config = StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true)
            .with_sse_keep_alive(None)
            .with_cancellation_token(cancel);
        let service: McpService = StreamableHttpService::new(
            || Ok(Server::new()),
            Arc::new(LocalSessionManager::default()),
            config,
        );
        Self {
            gate,
            service,
            oauth,
        }
    }

    fn router(self) -> Router {
        Router::new()
            .route("/healthz", get(healthz))
            .fallback(dispatch)
            .with_state(self)
    }
}

async fn healthz() -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )],
        r#"{"status":"ok","transport":"streamable-http","stateful":false}"#,
    )
}

async fn dispatch(State(app): State<App>, mut req: Request) -> axum::response::Response {
    // `uri()` and `headers_mut()` borrow the same request.
    let path = req.uri().path().trim_end_matches('/').to_owned();
    if req.headers().get(axum::http::header::HOST).is_none() {
        if let Some(authority) = req.uri().authority() {
            if let Ok(value) = axum::http::HeaderValue::from_str(authority.as_str()) {
                req.headers_mut().insert(axum::http::header::HOST, value);
            }
        }
    }
    if is_metadata_path(&path) {
        return metadata_response(&app, req.method(), req.headers());
    }
    let wanted = app.gate.path.trim_end_matches('/');
    if path != wanted {
        return (
            axum::http::StatusCode::NOT_FOUND,
            "MCP endpoint is the configured path, default /mcp",
        )
            .into_response();
    }
    if req.method() != axum::http::Method::POST {
        return (
            axum::http::StatusCode::METHOD_NOT_ALLOWED,
            [(axum::http::header::ALLOW, "POST")],
            "Streamable HTTP in stateless mode accepts POST",
        )
            .into_response();
    }
    if let Err(deny) = app.gate.decide(req.headers()) {
        return deny_response(&app, deny);
    }
    if app.gate.auth == HttpAuthMode::Oauth {
        let token = inbound_api_key(req.headers()).unwrap_or_default();
        let accepted = match app.oauth.as_ref().and_then(|state| state.verifier.as_ref()) {
            Some(verifier) => verifier.verify(&token).await.is_ok(),
            None => false,
        };
        if !accepted {
            return deny_response(&app, GateDeny::Unauthorized);
        }
    }
    let (parts, body) = app.service.handle(req).await.into_parts();
    axum::response::Response::from_parts(parts, axum::body::Body::new(body))
}

fn is_metadata_path(path: &str) -> bool {
    path == "/.well-known/oauth-protected-resource"
        || path.starts_with("/.well-known/oauth-protected-resource/")
}

fn metadata_response(
    app: &App,
    method: &axum::http::Method,
    headers: &axum::http::HeaderMap,
) -> axum::response::Response {
    if method != axum::http::Method::GET {
        return (
            axum::http::StatusCode::METHOD_NOT_ALLOWED,
            [(axum::http::header::ALLOW, "GET")],
        )
            .into_response();
    }
    if let Err(deny) = host_and_origin(app, headers) {
        return deny_response(app, deny);
    }
    let Some(policy) = app.oauth.as_ref().map(|state| &state.policy) else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )],
        policy.metadata().to_string(),
    )
        .into_response()
}

fn host_and_origin(app: &App, headers: &axum::http::HeaderMap) -> Result<(), GateDeny> {
    let mut open = app.gate.clone();
    open.auth = HttpAuthMode::None;
    open.decide(headers)
}

fn deny_response(app: &App, deny: GateDeny) -> axum::response::Response {
    let (status, body, challenge) = match deny {
        GateDeny::Unauthorized => (
            axum::http::StatusCode::UNAUTHORIZED,
            if app.gate.auth == HttpAuthMode::Oauth {
                "Sign in is required"
            } else {
                "Authorization: Bearer with an API key is required"
            },
            true,
        ),
        GateDeny::ForbiddenHost => (
            axum::http::StatusCode::FORBIDDEN,
            "Host is not in OOKCITE_MCP_ALLOWED_HOSTS",
            false,
        ),
        GateDeny::ForbiddenOrigin => (
            axum::http::StatusCode::FORBIDDEN,
            "Origin is not in OOKCITE_MCP_ALLOWED_ORIGINS",
            false,
        ),
    };
    let mut response = axum::response::Response::builder().status(status);
    if challenge {
        let value = app
            .oauth
            .as_ref()
            .map(|state| state.policy.challenge())
            .unwrap_or_else(|| "Bearer realm=\"ookcite\"".into());
        response = response.header(axum::http::header::WWW_AUTHENTICATE, value);
    }
    response
        .header(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

pub async fn serve(bind: &str) -> anyhow::Result<()> {
    let gate = Gate::from_env().map_err(|err| anyhow::anyhow!(err))?;
    let oauth = match gate.auth {
        HttpAuthMode::Oauth => {
            let verifier = Arc::new(OidcVerifier::from_env().map_err(|err| anyhow::anyhow!(err))?);
            Some(OauthState {
                policy: verifier.policy().clone(),
                verifier: Some(verifier),
            })
        }
        _ => None,
    };
    let cancel = CancellationToken::new();
    let shutdown = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown.cancel();
    });
    let app = App::new(gate.clone(), cancel.clone(), oauth);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let addr = listener.local_addr()?;
    eprintln!(
        "ookcite-mcp: streamable http http://{addr}{} auth={}",
        gate.path,
        match gate.auth {
            HttpAuthMode::Bearer => "bearer",
            HttpAuthMode::None => "none",
            HttpAuthMode::Oauth => "oauth",
        }
    );
    axum::serve(listener, app.router())
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbound_auth::HttpAuthMode;
    use crate::oidc_resource::OidcPolicy;

    const INIT_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#;

    fn test_gate(auth: HttpAuthMode) -> Gate {
        Gate {
            auth,
            origins: vec!["https://chat.example".into()],
            hosts: Gate::loopback_hosts(),
            path: "/mcp".into(),
        }
    }

    async fn spawn(auth: HttpAuthMode) -> (String, CancellationToken) {
        let cancel = CancellationToken::new();
        let oauth = (auth == HttpAuthMode::Oauth).then(|| OauthState {
            policy: OidcPolicy {
                issuer: "https://id.example".into(),
                audience: "https://api.example".into(),
                scope: "openid".into(),
                resource: "https://api.example".into(),
            },
            verifier: None,
        });
        let app = App::new(test_gate(auth), cancel.clone(), oauth);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let shutdown = cancel.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.router())
                .with_graceful_shutdown(async move { shutdown.cancelled().await })
                .await;
        });
        for _ in 0..50 {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        (format!("http://{addr}/mcp"), cancel)
    }

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    #[tokio::test]
    async fn bearer_mode_rejects_a_missing_key() {
        let (url, cancel) = spawn(HttpAuthMode::Bearer).await;
        let response = client().post(&url).body(INIT_BODY).send().await.unwrap();
        assert_eq!(response.status(), 401);
        assert!(response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .is_some());
        cancel.cancel();
    }

    #[tokio::test]
    async fn stateless_initialize_returns_json() {
        let (url, cancel) = spawn(HttpAuthMode::Bearer).await;
        let response = client()
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-03-26")
            .header("authorization", "Bearer ookc_test")
            .body(INIT_BODY)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = response.text().await.unwrap();
        assert_eq!(status, 200, "{body}");
        assert!(
            content_type.contains("application/json"),
            "content-type {content_type}, body {body}"
        );
        assert!(body.contains("\"result\""), "{body}");
        cancel.cancel();
    }

    #[tokio::test]
    async fn listed_origin_passes_and_other_origins_do_not() {
        let (url, cancel) = spawn(HttpAuthMode::None).await;
        let blocked = client()
            .post(&url)
            .header("origin", "https://evil.example")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(INIT_BODY)
            .send()
            .await
            .unwrap();
        assert_eq!(blocked.status(), 403);

        let allowed = client()
            .post(&url)
            .header("origin", "https://chat.example")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(INIT_BODY)
            .send()
            .await
            .unwrap();
        assert_eq!(allowed.status(), 200);
        cancel.cancel();
    }

    #[tokio::test]
    async fn get_is_not_the_legacy_sse_transport() {
        let (url, cancel) = spawn(HttpAuthMode::None).await;
        let response = client().get(&url).send().await.unwrap();
        assert_eq!(response.status(), 405);
        cancel.cancel();
    }

    #[tokio::test]
    async fn oauth_rejects_a_missing_token_and_an_api_key() {
        let (url, cancel) = spawn(HttpAuthMode::Oauth).await;
        let missing = client().post(&url).body(INIT_BODY).send().await.unwrap();
        assert_eq!(missing.status(), 401);
        let challenge = missing
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        assert!(challenge.contains("resource_metadata="), "{challenge}");

        let pasted = client()
            .post(&url)
            .header("authorization", "Bearer ookc_live")
            .body(INIT_BODY)
            .send()
            .await
            .unwrap();
        assert_eq!(pasted.status(), 401);

        let base = url.trim_end_matches("/mcp");
        let metadata = client()
            .get(format!("{base}/.well-known/oauth-protected-resource"))
            .send()
            .await
            .unwrap();
        assert_eq!(metadata.status(), 200);
        let body = metadata.text().await.unwrap();
        assert!(body.contains("authorization_servers"), "{body}");
        cancel.cancel();
    }
}

use std::io::Read as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ookcite_mcp::connect::{
    ConnectError, ConnectSecrets, DashboardClient, DevicePoll, LoopbackListener, Pkce,
    StartBrowserRequest, StartDeviceRequest, callback_state_matches, generate_pkce,
    random_journey_id, random_token, redact_diagnostic, verify_readiness,
};
use ookcite_mcp::credentials::{
    CredentialConfig, CredentialReference, CredentialSink, CredentialSource, KeyringBackend,
    PlatformCredentialSink, ProtectedFileSink, StoreCommandSink, choose_source, load_credential,
};
use secrecy::{ExposeSecret as _, SecretString};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const KEY: &str = "ookc_example-secret-value";

#[test]
fn pkce_uses_the_rfc_7636_s256_challenge() {
    let pkce = Pkce::from_verifier(VERIFIER);
    assert_eq!(pkce.challenge, CHALLENGE);
}

#[test]
fn callback_state_requires_an_exact_constant_time_match() {
    assert!(callback_state_matches("random-state", "random-state"));
    assert!(!callback_state_matches("random-state", "random-state-extra"));
    assert!(!callback_state_matches("random-state", "other-state"));
}

#[tokio::test]
async fn loopback_listener_uses_a_random_localhost_port() {
    let listener = LoopbackListener::bind(0).await.unwrap();
    let address = listener.local_addr().unwrap();
    assert_eq!(address.ip(), std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    assert_ne!(address.port(), 0);
}

#[tokio::test]
async fn occupied_callback_ports_are_rejected() {
    let occupied = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = occupied.local_addr().unwrap().port();
    assert!(LoopbackListener::bind(port).await.is_err());
}

#[test]
fn protocol_debug_and_errors_redact_every_secret() {
    let secrets = ConnectSecrets {
        state: "state-secret-123456".into(),
        verifier: VERIFIER.into(),
        authorization_code: Some("exchange-secret-123456".into()),
    };
    let rendered = format!("{secrets:?}");
    let diagnostic = redact_diagnostic(
        &format!(
            "state={} verifier={} code={}",
            secrets.state,
            secrets.verifier,
            secrets.authorization_code.as_deref().unwrap()
        ),
        &secrets,
    );
    for secret in [
        secrets.state.as_str(),
        secrets.verifier.as_str(),
        secrets.authorization_code.as_deref().unwrap(),
    ] {
        assert!(!rendered.contains(secret));
        assert!(!diagnostic.contains(secret));
        assert!(!diagnostic.contains(&secret[..secret.len().min(12)]));
    }
}

struct RecordingSink {
    received: Mutex<Option<String>>,
}

impl CredentialSink for RecordingSink {
    fn store(&self, secret: &SecretString) -> anyhow::Result<CredentialReference> {
        *self.received.lock().unwrap() = Some(secret.expose_secret().to_string());
        Ok(CredentialReference::Platform {
            service: "ookcite-mcp".into(),
            account: "default".into(),
        })
    }
}

#[test]
fn credential_sink_receives_the_secret_only_in_memory() {
    let sink = RecordingSink {
        received: Mutex::new(None),
    };
    let reference = sink.store(&SecretString::new(KEY.into())).unwrap();
    assert_eq!(sink.received.lock().unwrap().as_deref(), Some(KEY));
    assert!(!format!("{reference:?}").contains(KEY));
}

#[test]
fn store_command_receives_the_secret_on_standard_input() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("stored");
    let script = format!("read value; printf %s \"$value\" > {}", output.display());
    let sink = StoreCommandSink {
        command: vec!["sh".into(), "-c".into(), script],
        retrieve_command: "secret-tool lookup service ookcite".into(),
    };
    let reference = sink.store(&SecretString::new(KEY.into())).unwrap();

    let mut stored = String::new();
    std::fs::File::open(output)
        .unwrap()
        .read_to_string(&mut stored)
        .unwrap();
    assert_eq!(stored, KEY);
    assert!(!sink.command.iter().any(|argument| argument.contains(KEY)));
    assert_eq!(
        reference.config_env(),
        vec![(
            "OOKCITE_API_KEY_COMMAND".into(),
            "secret-tool lookup service ookcite".into()
        )]
    );
}

#[test]
#[cfg(unix)]
fn protected_file_is_owner_only_and_refuses_overwrite() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credential");
    let sink = ProtectedFileSink::new(path.clone());
    let reference = sink.store(&SecretString::new(KEY.into())).unwrap();

    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{KEY}\n"));
    assert!(sink.store(&SecretString::new("replacement".into())).is_err());
    assert_eq!(
        reference.config_env(),
        vec![(
            "OOKCITE_API_KEY_FILE".into(),
            path.to_string_lossy().into_owned()
        )]
    );
}

#[test]
fn existing_credential_sources_keep_their_precedence() {
    assert_eq!(
        choose_source(Some(KEY), Some("helper"), Some("file"), true),
        Some(CredentialSource::Environment)
    );
    assert_eq!(
        choose_source(None, Some("helper"), Some("file"), true),
        Some(CredentialSource::Command)
    );
    assert_eq!(
        choose_source(None, None, Some("file"), true),
        Some(CredentialSource::File)
    );
    assert_eq!(
        choose_source(None, None, None, true),
        Some(CredentialSource::Platform)
    );
}

#[test]
fn configuration_references_never_contain_the_secret() {
    for reference in [
        CredentialReference::Platform {
            service: "ookcite-mcp".into(),
            account: "default".into(),
        },
        CredentialReference::Command {
            retrieve_command: "secret-tool lookup service ookcite".into(),
        },
        CredentialReference::File {
            path: "/tmp/credential".into(),
        },
    ] {
        let rendered = format!("{reference:?} {:?}", reference.config_env());
        assert!(!rendered.contains(KEY));
    }
}

#[derive(Default)]
struct MemoryKeyring {
    entries: Mutex<HashMap<(String, String), String>>,
}

impl MemoryKeyring {
    fn password(&self, service: &str, account: &str) -> Option<String> {
        self.entries
            .lock()
            .unwrap()
            .get(&(service.into(), account.into()))
            .cloned()
    }
}

impl KeyringBackend for MemoryKeyring {
    fn get_password(&self, service: &str, account: &str) -> anyhow::Result<Option<SecretString>> {
        Ok(self
            .password(service, account)
            .map(SecretString::new))
    }

    fn set_password(
        &self,
        service: &str,
        account: &str,
        secret: &SecretString,
    ) -> anyhow::Result<()> {
        self.entries.lock().unwrap().insert(
            (service.into(), account.into()),
            secret.expose_secret().to_string(),
        );
        Ok(())
    }

    fn delete_password(&self, service: &str, account: &str) -> anyhow::Result<()> {
        self.entries
            .lock()
            .unwrap()
            .remove(&(service.into(), account.into()));
        Ok(())
    }
}

#[test]
fn platform_sink_refuses_overwrite_and_rolls_back_only_its_change() {
    let backend = MemoryKeyring::default();
    backend
        .set_password(
            "ookcite-mcp",
            "default",
            &SecretString::new("existing-key".into()),
        )
        .unwrap();

    let refusing = PlatformCredentialSink::new(&backend, "ookcite-mcp", "default");
    assert!(refusing.store(&SecretString::new(KEY.into())).is_err());
    assert_eq!(
        backend.password("ookcite-mcp", "default").as_deref(),
        Some("existing-key")
    );

    let replacing = PlatformCredentialSink::new(&backend, "ookcite-mcp", "default")
        .allow_replace(true);
    let reference = replacing.store(&SecretString::new(KEY.into())).unwrap();
    assert_eq!(
        backend.password("ookcite-mcp", "default").as_deref(),
        Some(KEY)
    );
    replacing.cleanup(&reference).unwrap();
    assert_eq!(
        backend.password("ookcite-mcp", "default").as_deref(),
        Some("existing-key")
    );

    let empty_backend = MemoryKeyring::default();
    let creating = PlatformCredentialSink::new(&empty_backend, "ookcite-mcp", "default");
    let reference = creating.store(&SecretString::new(KEY.into())).unwrap();
    creating.cleanup(&reference).unwrap();
    assert_eq!(empty_backend.password("ookcite-mcp", "default"), None);
}

#[tokio::test]
async fn credential_loader_preserves_environment_command_file_platform_precedence() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("credential");
    std::fs::write(&file, "file-key\n").unwrap();
    let backend = Arc::new(MemoryKeyring::default());
    backend
        .set_password(
            "ookcite-mcp",
            "default",
            &SecretString::new("platform-key".into()),
        )
        .unwrap();
    let mut config = CredentialConfig {
        api_key: Some(SecretString::new("environment-key".into())),
        command: Some("printf command-key".into()),
        file: Some(file),
        platform: Some(("ookcite-mcp".into(), "default".into())),
        command_timeout: Duration::from_secs(1),
    };

    assert_eq!(
        load_credential(&config, backend.as_ref())
            .await
            .unwrap()
            .unwrap()
            .expose_secret(),
        "environment-key"
    );
    config.api_key = None;
    assert_eq!(
        load_credential(&config, backend.as_ref())
            .await
            .unwrap()
            .unwrap()
            .expose_secret(),
        "command-key"
    );
    config.command = None;
    assert_eq!(
        load_credential(&config, backend.as_ref())
            .await
            .unwrap()
            .unwrap()
            .expose_secret(),
        "file-key"
    );
    config.file = None;
    assert_eq!(
        load_credential(&config, backend.as_ref())
            .await
            .unwrap()
            .unwrap()
            .expose_secret(),
        "platform-key"
    );
}

#[tokio::test]
async fn retrieval_command_is_bounded_and_never_echoes_secret_failures() {
    let backend = MemoryKeyring::default();
    let failure = CredentialConfig {
        api_key: None,
        command: Some(format!("printf {KEY}; exit 9")),
        file: None,
        platform: None,
        command_timeout: Duration::from_secs(1),
    };
    let diagnostic = load_credential(&failure, &backend)
        .await
        .unwrap_err()
        .to_string();
    assert!(!diagnostic.contains(KEY));
    assert!(!diagnostic.contains(&KEY[..12]));

    let timeout = CredentialConfig {
        api_key: None,
        command: Some("sleep 5".into()),
        file: None,
        platform: None,
        command_timeout: Duration::from_millis(50),
    };
    assert!(load_credential(&timeout, &backend).await.is_err());
}

#[test]
fn protected_file_cleanup_removes_only_the_file_created_by_the_sink() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credential");
    let sink = ProtectedFileSink::new(path.clone());
    let reference = sink.store(&SecretString::new(KEY.into())).unwrap();
    sink.cleanup(&reference).unwrap();
    assert!(!path.exists());

    std::fs::write(&path, "user-owned\n").unwrap();
    assert!(sink.store(&SecretString::new(KEY.into())).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "user-owned\n");
}

#[tokio::test]
async fn browser_start_uses_random_pkce_state_journey_and_loopback_port() {
    let dashboard = MockServer::start().await;
    let listener = LoopbackListener::bind(0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let pkce = generate_pkce();
    let state = random_token();
    let journey = random_journey_id();
    assert_eq!(pkce.verifier.len(), 43);
    assert_eq!(pkce.challenge.len(), 43);
    assert_eq!(state.len(), 43);
    assert!(uuid::Uuid::parse_str(&journey).is_ok());

    Mock::given(method("POST"))
        .and(path("/mcp/ookcite/authorize"))
        .and(body_json(serde_json::json!({
            "journey_id": journey,
            "code_challenge": pkce.challenge,
            "code_challenge_method": "S256",
            "state": state,
            "callback_port": port
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "authorization_id": "authorization-1",
            "authorization_url": "https://my.turtletech.us/connect/ookcite-mcp?authorization_id=authorization-1",
            "expires_in": 300
        })))
        .mount(&dashboard)
        .await;

    let response = DashboardClient::new(dashboard.uri())
        .start_browser(StartBrowserRequest {
            journey_id: &journey,
            code_challenge: &pkce.challenge,
            code_challenge_method: "S256",
            state: &state,
            callback_port: port,
        })
        .await
        .unwrap();
    assert_eq!(response.expires_in, 300);
    assert!(response.authorization_url.starts_with("https://my.turtletech.us/"));
}

#[tokio::test]
async fn loopback_callback_validates_state_and_returns_only_the_code() {
    let listener = LoopbackListener::bind(0).await.unwrap();
    let address = listener.local_addr().unwrap();
    let state = "random-state-value";
    let sender = tokio::spawn(async move {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        use tokio::io::AsyncWriteExt as _;
        stream
            .write_all(
                b"GET /callback?code=authorization-code&state=random-state-value HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            )
            .await
            .unwrap();
    });
    let code = listener
        .wait_for_callback(state, std::time::Duration::from_secs(2))
        .await
        .unwrap();
    sender.await.unwrap();
    assert_eq!(code, "authorization-code");

    let wrong = LoopbackListener::bind(0).await.unwrap();
    let wrong_address = wrong.local_addr().unwrap();
    tokio::spawn(async move {
        let mut stream = tokio::net::TcpStream::connect(wrong_address).await.unwrap();
        use tokio::io::AsyncWriteExt as _;
        stream
            .write_all(
                b"GET /callback?code=authorization-code&state=wrong HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            )
            .await
            .unwrap();
    });
    assert_eq!(
        wrong
            .wait_for_callback(state, std::time::Duration::from_secs(2))
            .await
            .unwrap_err(),
        ConnectError::InvalidState
    );
}

#[tokio::test]
async fn device_start_poll_and_denial_preserve_server_status() {
    let dashboard = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp/ookcite/device"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "device_code": "device-code",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://my.turtletech.us/connect/ookcite-mcp?user_code=ABCD-EFGH",
            "expires_in": 600,
            "interval": 5
        })))
        .mount(&dashboard)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp/ookcite/device/poll"))
        .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({
            "status": "pending"
        })))
        .mount(&dashboard)
        .await;
    let client = DashboardClient::new(dashboard.uri());
    let started = client
        .start_device(StartDeviceRequest {
            journey_id: "018f47e2-19c3-7b8a-8f62-62fe39151ec4",
            code_challenge: CHALLENGE,
            code_challenge_method: "S256",
            state: "state-value-123456",
        })
        .await
        .unwrap();
    assert_eq!(started.expires_in, 600);
    assert_eq!(
        client
            .poll_device(&started.device_code, "state-value-123456")
            .await
            .unwrap(),
        DevicePoll::Pending
    );

    let denied = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp/ookcite/device/poll"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error": "access_denied"
        })))
        .mount(&denied)
        .await;
    assert_eq!(
        DashboardClient::new(denied.uri())
            .poll_device("device-code", "state-value-123456")
            .await
            .unwrap_err(),
        ConnectError::Denied
    );
}

#[tokio::test]
async fn exchange_readiness_and_receipt_complete_without_diagnostic_leakage() {
    let dashboard = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp/ookcite/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "api_key": KEY,
            "installation_receipt": "receipt-secret-value",
            "plan": "Free"
        })))
        .mount(&dashboard)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp/ookcite/receipt"))
        .and(body_json(serde_json::json!({"receipt": "receipt-secret-value"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "credential_installed": true
        })))
        .mount(&dashboard)
        .await;
    let client = DashboardClient::new(dashboard.uri());
    let exchanged = client
        .exchange("authorization-code", VERIFIER, "state-value-123456")
        .await
        .unwrap();
    assert_eq!(exchanged.credential.expose_secret(), KEY);
    assert!(!format!("{exchanged:?}").contains(KEY));

    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "authenticated": true,
            "plan": "Free",
            "lookups_remaining": 60,
            "lookups_limit": 60
        })))
        .mount(&api)
        .await;
    let readiness = verify_readiness(&api.uri(), &exchanged.credential)
        .await
        .unwrap();
    assert!(readiness.authenticated);
    assert_eq!(readiness.lookups_remaining, 60);
    client
        .redeem_receipt(&exchanged.installation_receipt)
        .await
        .unwrap();
}

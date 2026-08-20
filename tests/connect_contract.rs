use std::io::Read as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Mutex;

use ookcite_mcp::connect::{
    ConnectSecrets, LoopbackListener, Pkce, callback_state_matches, redact_diagnostic,
};
use ookcite_mcp::credentials::{
    CredentialReference, CredentialSink, CredentialSource, ProtectedFileSink, StoreCommandSink,
    choose_source,
};
use secrecy::{ExposeSecret as _, SecretString};

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
    let sink = ProtectedFileSink { path: path.clone() };
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

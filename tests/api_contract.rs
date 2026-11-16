//! Contract test: every endpoint in `endpoints::ALL` must exist in the
//! ttech-cite OpenAPI spec.
//!
//! The spec is stored encrypted at `contract/openapi.json.age` so the public
//! ookcite-mcp repo does not leak the private ttech-cite API surface. The
//! test decrypts in memory using `OOKCITE_CONTRACT_KEY` (pass-stored locally,
//! GitHub secret in CI) and never writes plaintext to disk.
//!
//! If this test fails:
//!   - MCP-side: `endpoints::ALL` references a path/method that ttech-cite
//!     does not expose. Either fix the MCP tool to use a real endpoint, or
//!     add the endpoint to ttech-cite and regenerate via `./contract/regen.sh`.
//!   - Server-side: ttech-cite renamed or removed an endpoint the MCP uses.
//!     Update `endpoints::ALL` (and any affected tools), regenerate.

use std::io::Read;

use ookcite_mcp::endpoints;
use serde_json::Value;

const ENCRYPTED_SPEC: &[u8] = include_bytes!("../contract/openapi.json.age");

fn contract_key() -> Option<String> {
    std::env::var("OOKCITE_CONTRACT_KEY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn decrypt_spec() -> Option<Value> {
    let Some(key) = contract_key() else {
        eprintln!("skipped: OOKCITE_CONTRACT_KEY is unset");
        return None;
    };
    let decryptor = match age::Decryptor::new(ENCRYPTED_SPEC)
        .expect("openapi.json.age is corrupt or not an age file")
    {
        age::Decryptor::Passphrase(d) => d,
        age::Decryptor::Recipients(_) => {
            panic!("openapi.json.age was encrypted to recipients, not a passphrase")
        }
    };

    let mut reader = decryptor
        .decrypt(&secrecy::SecretString::new(key), None)
        .expect("decryption failed -- wrong OOKCITE_CONTRACT_KEY?");

    let mut decrypted = Vec::new();
    reader
        .read_to_end(&mut decrypted)
        .expect("failed to read decrypted stream");

    Some(serde_json::from_slice(&decrypted).expect("decrypted bytes are not valid JSON"))
}

#[test]
fn every_endpoint_exists_in_openapi_spec() {
    let Some(spec) = decrypt_spec() else {
        return;
    };
    let paths = spec["paths"]
        .as_object()
        .expect("OpenAPI spec has no `paths` object");

    let mut missing: Vec<String> = Vec::new();
    for ep in endpoints::ALL {
        let Some(path_item) = paths.get(ep.path) else {
            missing.push(format!("{} {} -- path not in spec", ep.method, ep.path));
            continue;
        };
        let method_key = ep.method.to_lowercase();
        if path_item.get(&method_key).is_none() {
            missing.push(format!(
                "{} {} -- path exists but method `{}` is not declared",
                ep.method, ep.path, method_key
            ));
        }
    }

    assert!(
        missing.is_empty(),
        "MCP endpoints missing from OpenAPI spec:\n  {}\n\n\
         If the MCP is correct and the spec is stale, regenerate via:\n  \
         ./contract/regen.sh",
        missing.join("\n  ")
    );
}

#[test]
fn openapi_spec_has_expected_shape() {
    let Some(spec) = decrypt_spec() else {
        return;
    };
    assert_eq!(
        spec["openapi"].as_str(),
        Some("3.1.0"),
        "expected OpenAPI 3.1"
    );
    assert_eq!(spec["info"]["title"].as_str(), Some("OokCite API"));
    assert!(spec["paths"].is_object(), "paths must be an object");
}

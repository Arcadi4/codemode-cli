//! Fixture tests for native credential parsing and decryption.
//!
//! These tests never read a keychain, a state database, or any user file.

use std::collections::BTreeMap;

use crate::credentials::{
    NativeToken, claude_account, claude_service_name, codex_account_keys,
    cursor_secret_key_matches, cursor_server_url_storage_key, cursor_updated_at_storage_key,
    decrypt_electron_safe_storage, parse_claude_store, parse_codex_store,
    parse_cursor_secret_value, parse_vscode_secret_value, parse_vscode_tokens,
    vscode_secret_key_matches,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};

const TARGET: &str = "https://mcp.example.com/mcp";
const OTHER: &str = "https://other.example/mcp";

// Generated from Chromium's macOS OSCrypt parameters:
// PBKDF2-HMAC-SHA1("fixture-safe-storage-password", "saltysalt", 1003, 16)
// then AES-128-CBC with sixteen ASCII spaces and the "v10" prefix.
const VSCODE_PASSWORD: &str = "fixture-safe-storage-password";
const VSCODE_TOKEN: &str = "fixture-vscode-token";
const VSCODE_VALUE: &str = r#"{"type":"Buffer","data":[118,49,48,123,102,33,132,245,213,190,164,176,222,106,176,19,216,26,85,187,154,79,26,89,28,125,36,168,29,72,213,95,22,2,125,128,206,174,172,104,87,133,192,49,165,111,183,20,128,236,45,159,233,92,46,135,123,231,161,208,112,133,244,146,61,101,168,88,149,213,255,239,53,146,203,200,150,63,231,140,7,114,52,106,201,160,209,43,121,211,31,184,234,91,2,17,33,16,71,183,245,94,234,129,142,240,248,251,242,200,201,187,224,99,94]}"#;
const CURSOR_TOKEN: &str = "fixture-cursor-token";
const CURSOR_VALUE: &str = r#"{"type":"Buffer","data":[118,49,48,6,45,120,156,172,219,58,199,107,15,182,104,23,114,209,73,43,202,33,251,126,84,210,47,15,246,174,94,202,170,183,125,111,81,180,107,181,117,134,110,231,175,113,121,140,84,88,114,223,216,137,196,191,145,197,246,120,196,38,244,7,207,238,49,231,117,100,219,150,36,110,59,71,37,189,159,94,236,1,145,21,31,38,110,223,1,146,53,173,91,242,62,80,224,219,203]}"#;

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect()
}

fn claude_store(entries: Value) -> String {
    json!({ "mcpOAuth": entries }).to_string()
}

fn vscode_key(provider: &str) -> String {
    format!(
        "secret://{}",
        json!({
            "isDynamicAuthProvider": true,
            "authProviderId": provider,
            "clientId": "fixture-client",
        })
    )
}

fn cursor_outer_key(inner: &str) -> String {
    format!("mcpOAuth.secret.{}", URL_SAFE_NO_PAD.encode(inner))
}

fn cursor_url_inner(key: &str, url: &str) -> String {
    format!("{key}[url:{}] mcp_tokens", URL_SAFE_NO_PAD.encode(url))
}

#[test]
fn claude_service_name_tracks_config_directory_and_ignores_empty_overrides() {
    assert_eq!(claude_service_name(&env(&[])), "Claude Code-credentials");
    assert_eq!(
        claude_service_name(&env(&[("CLAUDE_CONFIG_DIR", "/tmp/custom-claude")])),
        "Claude Code-credentials-5204cebd"
    );
    assert_eq!(
        claude_service_name(&env(&[(
            "CLAUDE_SECURESTORAGE_CONFIG_DIR",
            "/tmp/custom-claude"
        )])),
        "Claude Code-credentials-5204cebd"
    );
    assert_eq!(
        claude_service_name(&env(&[("CLAUDE_CONFIG_DIR", "")])),
        "Claude Code-credentials"
    );
    assert_eq!(
        claude_service_name(&env(&[("CLAUDE_SECURESTORAGE_CONFIG_DIR", "")])),
        "Claude Code-credentials"
    );
}

#[test]
fn claude_account_sanitizes_invalid_user_names() {
    assert_eq!(claude_account(&env(&[("USER", "skylar")])), "skylar");
    assert_eq!(
        claude_account(&env(&[("USER", "bad user")])),
        "claude-code-user"
    );
    assert_eq!(
        claude_account(&env(&[("LOGNAME", "fallback-login")])),
        "fallback-login"
    );
}

#[test]
fn claude_uses_only_the_matching_server_url() {
    let raw = claude_store(json!({
        "wrong": {
            "serverUrl": OTHER,
            "accessToken": "wrong-origin-secret",
            "expiresAt": 9_999_999_999_999_u64
        },
        "wanted": {
            "serverUrl": "https://mcp.example.com/mcp/",
            "accessToken": "claude-secret",
            "expiresAt": 9_999_999_999_999_u64
        }
    }));

    assert!(parse_claude_store(&raw, TARGET).unwrap().is_none());
    let token = parse_claude_store(&raw, "https://mcp.example.com/mcp/")
        .unwrap()
        .unwrap();
    assert_eq!(token.access_token, "claude-secret");
    assert_eq!(token.expires_at, Some(9_999_999_999));
    assert_eq!(token.source, "Claude Code");
    assert!(!format!("{token:?}").contains("claude-secret"));
}

#[test]
fn claude_prefers_a_live_entry_and_returns_expired_only_as_a_last_resort() {
    let live = claude_store(json!({
        "expired": {
            "serverUrl": TARGET,
            "accessToken": "expired-claude-secret",
            "expiresAt": 1_700_000_000_000_u64
        },
        "live": {
            "serverUrl": TARGET,
            "accessToken": "live-claude-secret"
        }
    }));
    let token = parse_claude_store(&live, TARGET).unwrap().unwrap();
    assert_eq!(token.access_token, "live-claude-secret");
    assert_eq!(token.expires_at, None);

    let expired = claude_store(json!({
        "expired": {
            "serverUrl": TARGET,
            "accessToken": "expired-claude-secret",
            "expiresAt": 1_700_000_000_000_u64
        }
    }));
    let token = parse_claude_store(&expired, TARGET).unwrap().unwrap();
    assert_eq!(token.access_token, "expired-claude-secret");
    assert_eq!(token.expires_at, Some(1_700_000_000));
}

#[test]
fn claude_ignores_unrelated_malformed_entries_but_errors_on_matching_ones() {
    let unrelated = claude_store(json!({
        "unrelated": { "serverUrl": OTHER }
    }));
    assert!(parse_claude_store(&unrelated, TARGET).unwrap().is_none());

    let missing = claude_store(json!({
        "wanted": { "serverUrl": TARGET, "expiresAt": 9_999_999_999_999_u64 }
    }));
    let error = parse_claude_store(&missing, TARGET).unwrap_err();
    assert!(!error.to_string().contains("wanted"));

    let empty = claude_store(json!({
        "wanted": { "serverUrl": TARGET, "accessToken": "" }
    }));
    assert!(parse_claude_store(&empty, TARGET).is_err());

    let malformed_expiry = claude_store(json!({
        "wanted": { "serverUrl": TARGET, "accessToken": "secret", "expiresAt": "soon" }
    }));
    assert!(parse_claude_store(&malformed_expiry, TARGET).is_err());

    let malformed_document = json!({ "mcpOAuth": [] }).to_string();
    assert!(parse_claude_store(&malformed_document, TARGET).is_err());
    assert!(parse_claude_store("[]", TARGET).is_err());
}

#[test]
fn codex_account_keys_match_both_serde_json_map_orderings() {
    // Codex's payload is {"type":"http","url":...,"headers":{}}. Without
    // serde_json/preserve_order the keys serialize sorted; builds with that
    // feature use insertion order.
    assert_eq!(
        codex_account_keys("github", TARGET).unwrap(),
        vec![
            "github|a61b5eaf39b93a51".to_string(),
            "github|4282f2a9112820d2".to_string(),
        ]
    );
    assert_eq!(
        codex_account_keys("local:github", TARGET).unwrap(),
        codex_account_keys("github", TARGET).unwrap()
    );
    assert_ne!(
        codex_account_keys("github", TARGET).unwrap(),
        codex_account_keys("github", OTHER).unwrap()
    );
    assert!(codex_account_keys("executor:internal", TARGET).is_none());
    assert!(codex_account_keys("ema-idp:internal", TARGET).is_none());
}

#[test]
fn codex_record_url_must_match_and_token_response_is_supported() {
    let record = json!({
        "server_name": "github",
        "url": TARGET,
        "client_id": "fixture-client",
        "token_response": {
            "access_token": "codex-secret",
            "token_type": "Bearer"
        },
        "expires_at": 9_999_999_999_999_u64
    })
    .to_string();

    let token = parse_codex_store(&record, TARGET).unwrap().unwrap();
    assert_eq!(token.access_token, "codex-secret");
    assert_eq!(token.expires_at, Some(9_999_999_999));
    assert_eq!(token.source, "Codex");
    assert!(!format!("{token:?}").contains("codex-secret"));

    assert!(parse_codex_store(&record, OTHER).unwrap().is_none());

    let top_level = json!({
        "server_url": TARGET,
        "access_token": "codex-top-level-secret",
        "expires_at": 1_700_000_000_000_u64
    })
    .to_string();
    let token = parse_codex_store(&top_level, TARGET).unwrap().unwrap();
    assert_eq!(token.access_token, "codex-top-level-secret");
    assert_eq!(token.expires_at, Some(1_700_000_000));
}

#[test]
fn codex_matching_record_without_token_is_an_error() {
    let record = json!({
        "url": TARGET,
        "token_response": { "token_type": "Bearer" }
    })
    .to_string();
    let error = parse_codex_store(&record, TARGET).unwrap_err();
    assert!(!error.to_string().contains("Bearer"));
}

#[test]
fn vscode_secret_keys_are_bound_to_the_resource_url() {
    let key = vscode_key(&format!("https://auth.example.com {TARGET}"));
    assert!(vscode_secret_key_matches(&key, TARGET));
    assert!(!vscode_secret_key_matches(
        &key,
        "https://mcp.example.com/mcp/"
    ));
    assert!(!vscode_secret_key_matches(&key, OTHER));
    assert!(!vscode_secret_key_matches(
        &key,
        "https://mcp.example.com/mcp?tenant=other"
    ));
    assert!(!vscode_secret_key_matches(
        &vscode_key("https://mcp.example.com/mcp"),
        TARGET
    ));
    assert!(!vscode_secret_key_matches(
        &vscode_key("https://auth.example.com"),
        TARGET
    ));
    assert!(!vscode_secret_key_matches(
        &format!(
            "secret://{}",
            json!({
                "extensionId": "vscode.github-authentication",
                "key": "github.auth"
            })
        ),
        TARGET
    ));
    assert!(!vscode_secret_key_matches(
        "not-a-secret://{\"isDynamicAuthProvider\":true}",
        TARGET
    ));
}

#[test]
fn vscode_decrypts_the_chromium_safe_storage_fixture() {
    let token = parse_vscode_secret_value(VSCODE_VALUE.as_bytes(), VSCODE_PASSWORD)
        .unwrap()
        .unwrap();
    assert_eq!(token.access_token, VSCODE_TOKEN);
    assert_eq!(token.expires_at, Some(1_700_003_600));
    assert_eq!(token.source, "VS Code");
    assert!(!format!("{token:?}").contains(VSCODE_TOKEN));
}

#[test]
fn vscode_wrong_password_and_unknown_prefix_fail_without_exposing_secrets() {
    let error = parse_vscode_secret_value(VSCODE_VALUE.as_bytes(), "wrong-password").unwrap_err();
    let text = format!("{error:#}");
    assert!(!text.contains(VSCODE_TOKEN));

    let mut value: Value = serde_json::from_str(VSCODE_VALUE).unwrap();
    value["data"][2] = json!(b'1' as u64);
    let error =
        parse_vscode_secret_value(value.to_string().as_bytes(), VSCODE_PASSWORD).unwrap_err();
    assert!(error.to_string().contains("unsupported"));
    assert!(!error.to_string().contains(VSCODE_TOKEN));
}

#[test]
fn vscode_prefers_an_unexpired_session_and_returns_expired_if_that_is_all() {
    let sessions = json!([
        {
            "access_token": "expired-vscode-secret",
            "created_at": 1_700_000_000_000_u64,
            "expires_in": 1
        },
        {
            "access_token": "live-vscode-secret",
            "created_at": 1_700_000_000_000_u64,
            "expires_in": 999_999_999
        }
    ])
    .to_string();
    let token = parse_vscode_tokens(&sessions).unwrap().unwrap();
    assert_eq!(token.access_token, "live-vscode-secret");
    assert_eq!(token.expires_at, Some(2_699_999_999));

    let expired = json!([
        {
            "access_token": "expired-vscode-secret",
            "created_at": 1_700_000_000_000_u64,
            "expires_in": 1
        }
    ])
    .to_string();
    let token = parse_vscode_tokens(&expired).unwrap().unwrap();
    assert_eq!(token.access_token, "expired-vscode-secret");
    assert_eq!(token.expires_at, Some(1_700_000_001));
}

#[test]
fn cursor_secret_keys_include_the_url_scope_and_reject_name_only_entries() {
    let key = cursor_outer_key(&cursor_url_inner("", TARGET));
    assert!(cursor_secret_key_matches(&key, TARGET));
    assert!(!cursor_secret_key_matches(
        &key,
        "https://mcp.example.com/mcp/"
    ));
    assert!(!cursor_secret_key_matches(&key, OTHER));
    assert!(!cursor_secret_key_matches(
        &cursor_outer_key(&cursor_url_inner(
            "",
            "https://mcp.example.com/mcp?tenant=other"
        )),
        TARGET
    ));
    assert!(!cursor_secret_key_matches(
        &cursor_outer_key("[github] mcp_tokens"),
        TARGET
    ));
    assert!(!cursor_secret_key_matches(
        &cursor_outer_key("[server-id] mcp_tokens"),
        TARGET
    ));
    assert!(!cursor_secret_key_matches(
        "mcpOAuth.secret.not-base64!",
        TARGET
    ));

    let profile_key = cursor_outer_key(&format!(
        "[profile:ZGVmYXVsdA] {}",
        cursor_url_inner("", TARGET)
    ));
    assert!(cursor_secret_key_matches(&profile_key, TARGET));
}

#[test]
fn cursor_updated_at_key_keeps_the_profile_scope() {
    let key = cursor_outer_key(&format!(
        "[profile:ZGVmYXVsdA] {}",
        cursor_url_inner("", TARGET)
    ));
    let updated = cursor_updated_at_storage_key(&key).unwrap();
    assert!(updated.starts_with("mcpOAuth.global."));
    assert_eq!(
        updated,
        format!(
            "mcpOAuth.global.{}",
            URL_SAFE_NO_PAD.encode(format!(
                "[profile:ZGVmYXVsdA] [url:{}] mcp_oauth_updated_at_ms",
                URL_SAFE_NO_PAD.encode(TARGET)
            ))
        )
    );
    assert_eq!(
        cursor_updated_at_storage_key(&cursor_outer_key("[server-id] mcp_tokens")).unwrap(),
        format!(
            "mcpOAuth.global.{}",
            URL_SAFE_NO_PAD.encode("[server-id] mcp_oauth_updated_at_ms")
        )
    );
    assert_eq!(
        cursor_server_url_storage_key(&cursor_outer_key("[server-id] mcp_tokens")).unwrap(),
        format!(
            "mcpOAuth.global.{}",
            URL_SAFE_NO_PAD.encode("[server-id] mcp_server_url")
        )
    );
    assert!(
        cursor_server_url_storage_key(&cursor_outer_key(&cursor_url_inner("", TARGET))).is_none()
    );
}

#[test]
fn cursor_decrypts_the_vendor_safe_storage_fixture() {
    let token = parse_cursor_secret_value(
        CURSOR_VALUE.as_bytes(),
        VSCODE_PASSWORD,
        Some(1_700_000_000_000),
    )
    .unwrap()
    .unwrap();
    assert_eq!(token.access_token, CURSOR_TOKEN);
    assert_eq!(token.expires_at, Some(1_700_003_600));
    assert_eq!(token.source, "Cursor");
    assert!(!format!("{token:?}").contains(CURSOR_TOKEN));

    let unknown_lifetime =
        parse_cursor_secret_value(CURSOR_VALUE.as_bytes(), VSCODE_PASSWORD, None)
            .unwrap()
            .unwrap();
    assert_eq!(unknown_lifetime.access_token, CURSOR_TOKEN);
    assert_eq!(unknown_lifetime.expires_at, None);

    let error = parse_cursor_secret_value(
        CURSOR_VALUE.as_bytes(),
        "wrong-password",
        Some(1_700_000_000_000),
    )
    .unwrap_err();
    assert!(!format!("{error:#}").contains(CURSOR_TOKEN));
}

#[test]
fn native_token_debug_redacts_the_access_token() {
    let token = NativeToken {
        access_token: "top-secret".to_string(),
        expires_at: Some(42),
        source: "fixture",
    };
    let debug = format!("{token:?}");
    assert!(debug.contains("<redacted>"));
    assert!(debug.contains("fixture"));
    assert!(!debug.contains("top-secret"));
}

#[test]
fn electron_decryption_rejects_non_block_ciphertext() {
    let error = decrypt_electron_safe_storage(b"v10short", VSCODE_PASSWORD).unwrap_err();
    assert!(error.to_string().contains("malformed"));
}

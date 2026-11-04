//! Native stores must bind each token to the requested endpoint.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub struct NativeToken {
    pub access_token: String,
    /// Unix seconds, or `None` when the provider did not record a lifetime.
    pub expires_at: Option<u64>,
    pub source: &'static str,
}

impl fmt::Debug for NativeToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeToken")
            .field("source", &self.source)
            .field("access_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Look up a native OAuth token for exactly `url`.
///
/// Returns `Ok(None)` when no supported native store has a record for this
/// endpoint. Store-level failures (locked keychain, corrupt matching record,
/// unreadable database) are returned as errors rather than silently producing
/// an unauthenticated request.
pub fn lookup(
    server_name: &str,
    url: &str,
    env: &BTreeMap<String, String>,
) -> Result<Option<NativeToken>> {
    #[cfg(target_os = "macos")]
    {
        lookup_macos(server_name, url, env)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (server_name, url, env);
        bail!(
            "native credential lookup is implemented only for macOS keychains; \
             use an Authorization header, bearer_token_env_var, or a supported file store"
        )
    }
}

#[cfg(target_os = "macos")]
fn lookup_macos(
    server_name: &str,
    url: &str,
    env: &BTreeMap<String, String>,
) -> Result<Option<NativeToken>> {
    type Reader = fn(&str, &str, &BTreeMap<String, String>) -> Result<Option<NativeToken>>;
    let readers: [Reader; 4] = [
        claude_keychain_lookup,
        codex_keychain_lookup,
        vscode_lookup,
        cursor_lookup,
    ];

    let mut expired = None;
    let mut first_error = None;
    for reader in readers {
        match reader(server_name, url, env) {
            Ok(Some(token)) if token_is_expired(token.expires_at) => {
                if expired.is_none() {
                    expired = Some(token);
                }
            }
            Ok(Some(token)) => return Ok(Some(token)),
            Ok(None) => {}
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }

    if let Some(token) = expired {
        return Ok(Some(token));
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    Ok(None)
}

const CLAUDE_SERVICE: &str = "Claude Code-credentials";

/// Claude hashes an NFC-normalized config path; non-NFC custom paths may not match.
pub(crate) fn claude_service_name(env: &BTreeMap<String, String>) -> String {
    let configured = if let Some(value) = env.get("CLAUDE_SECURESTORAGE_CONFIG_DIR") {
        if value.is_empty() {
            None
        } else {
            Some(value.as_str())
        }
    } else {
        env.get("CLAUDE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(String::as_str)
    };

    match configured {
        Some(path) => format!("{CLAUDE_SERVICE}-{}", hash_prefix(path, 4)),
        None => CLAUDE_SERVICE.to_string(),
    }
}

pub(crate) fn claude_account(env: &BTreeMap<String, String>) -> String {
    let user = env
        .get("USER")
        .filter(|value| !value.is_empty())
        .or_else(|| env.get("LOGNAME").filter(|value| !value.is_empty()));

    match user {
        Some(user)
            if user
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')) =>
        {
            user.clone()
        }
        _ => "claude-code-user".to_string(),
    }
}

pub(crate) fn parse_claude_store(raw: &str, url: &str) -> Result<Option<NativeToken>> {
    let document: Value =
        serde_json::from_str(raw).context("Claude Code credential document is not valid JSON")?;
    let document = document
        .as_object()
        .ok_or_else(|| anyhow!("Claude Code credential document is not a JSON object"))?;
    let Some(entries) = document.get("mcpOAuth") else {
        return Ok(None);
    };
    let Some(entries) = entries.as_object() else {
        bail!("Claude Code credential document has a malformed 'mcpOAuth' field");
    };

    let mut expired = None;
    let mut malformed = None;
    for entry in entries.values() {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let Some(entry_url) = string_field(entry, &["serverUrl", "server_url"]) else {
            continue;
        };
        if !urls_match(entry_url, url) {
            continue;
        }

        match claude_token(entry) {
            Ok(token) if token_is_expired(token.expires_at) => {
                if expired.is_none() {
                    expired = Some(token);
                }
            }
            Ok(token) => return Ok(Some(token)),
            Err(error) => {
                if malformed.is_none() {
                    malformed = Some(error);
                }
            }
        }
    }

    if let Some(token) = expired {
        return Ok(Some(token));
    }
    if let Some(error) = malformed {
        return Err(error);
    }
    Ok(None)
}

fn claude_token(entry: &Map<String, Value>) -> Result<NativeToken> {
    let access_token = string_field(entry, &["accessToken", "access_token"])
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            anyhow!("Claude Code credential for the requested endpoint has no access token")
        })?;
    let expires_at = optional_timestamp_field(
        entry,
        &["expiresAt", "expires_at"],
        "Claude Code credential",
    )?;
    Ok(NativeToken {
        access_token: access_token.to_string(),
        expires_at,
        source: "Claude Code",
    })
}

const CODEX_SERVICE: &str = "Codex MCP Credentials";

/// Covers both serde_json map orderings Codex builds may hash; internal
/// executor/enterprise keys include private state and are unsupported.
pub(crate) fn codex_account_keys(server_name: &str, url: &str) -> Option<Vec<String>> {
    if server_name.starts_with("executor:") || server_name.starts_with("ema-idp:") {
        return None;
    }
    let server_name = server_name.strip_prefix("local:").unwrap_or(server_name);

    let encoded_url = serde_json::to_string(url).expect("serializing a string cannot fail");
    let sorted_payload = format!("{{\"headers\":{{}},\"type\":\"http\",\"url\":{encoded_url}}}");
    let insertion_payload = format!("{{\"type\":\"http\",\"url\":{encoded_url},\"headers\":{{}}}}");

    let mut keys = Vec::new();
    for payload in [sorted_payload, insertion_payload] {
        keys.push(format!("{server_name}|{}", hash_prefix(&payload, 8)));
    }
    keys.dedup();
    Some(keys)
}

pub(crate) fn parse_codex_store(raw: &str, url: &str) -> Result<Option<NativeToken>> {
    let document: Value =
        serde_json::from_str(raw).context("Codex credential record is not valid JSON")?;
    let entry = document
        .as_object()
        .ok_or_else(|| anyhow!("Codex credential record is not a JSON object"))?;

    let record_url = string_field(entry, &["url", "server_url"])
        .ok_or_else(|| anyhow!("Codex credential record is missing its endpoint URL"))?;
    if !urls_match(record_url, url) {
        return Ok(None);
    }

    let access_token = string_field(entry, &["access_token", "accessToken"])
        .filter(|token| !token.is_empty())
        .or_else(|| {
            entry
                .get("token_response")
                .and_then(Value::as_object)
                .and_then(|response| string_field(response, &["access_token"]))
                .filter(|token| !token.is_empty())
        })
        .ok_or_else(|| {
            anyhow!("Codex credential record for the requested endpoint has no access token")
        })?;
    let expires_at =
        optional_timestamp_field(entry, &["expires_at", "expiresAt"], "Codex credential")?;

    Ok(Some(NativeToken {
        access_token: access_token.to_string(),
        expires_at,
        source: "Codex",
    }))
}

const VSCODE_SAFE_STORAGE_SERVICE: &str = "Code Safe Storage";
const VSCODE_SAFE_STORAGE_ACCOUNT: &str = "Code";

pub(crate) fn vscode_secret_key_matches(key: &str, url: &str) -> bool {
    let Some(raw) = key.strip_prefix("secret://") else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return false;
    };
    if value.get("isDynamicAuthProvider").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    let Some(provider) = value.get("authProviderId").and_then(Value::as_str) else {
        return false;
    };
    let Some((_authorization_server, resource)) = provider.split_once(' ') else {
        return false;
    };
    !resource.is_empty() && urls_match(resource, url)
}

pub(crate) fn parse_vscode_secret_value(
    value: &[u8],
    safe_storage_password: &str,
) -> Result<Option<NativeToken>> {
    let ciphertext = electron_buffer_data(value)?;
    let plaintext = decrypt_electron_safe_storage(&ciphertext, safe_storage_password)?;
    parse_vscode_tokens(&plaintext)
}

fn electron_buffer_data(value: &[u8]) -> Result<Vec<u8>> {
    let document: Value =
        serde_json::from_slice(value).context("VS Code encrypted secret is not valid JSON")?;
    let data = document
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("VS Code encrypted secret has no byte array 'data' field"))?;

    let mut bytes = Vec::with_capacity(data.len());
    for value in data {
        let byte = value
            .as_u64()
            .filter(|byte| *byte <= u8::MAX as u64)
            .ok_or_else(|| anyhow!("VS Code encrypted secret contains an invalid byte"))?;
        bytes.push(byte as u8);
    }
    Ok(bytes)
}

/// Chromium v10: PBKDF2-HMAC-SHA1/1003, AES-128-CBC, space IV.
pub(crate) fn decrypt_electron_safe_storage(
    ciphertext: &[u8],
    safe_storage_password: &str,
) -> Result<String> {
    const PREFIX: &[u8] = b"v10";
    if !ciphertext.starts_with(PREFIX) {
        bail!(
            "unsupported Electron safe-storage ciphertext version; cannot read VS Code credentials"
        );
    }
    let ciphertext = &ciphertext[PREFIX.len()..];
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(16) {
        bail!("Electron safe-storage ciphertext is malformed");
    }

    let mut key = [0_u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(
        safe_storage_password.as_bytes(),
        b"saltysalt",
        1003,
        &mut key,
    );

    use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
    type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
    let iv = [b' '; 16];
    let mut buffer = ciphertext.to_vec();
    let plaintext = Aes128CbcDec::new((&key).into(), (&iv).into())
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .map_err(|_| {
            anyhow!("Electron safe-storage decryption failed; the stored safe-storage key does not match")
        })?;

    String::from_utf8(plaintext.to_vec()).context("Electron safe-storage plaintext is not UTF-8")
}

pub(crate) fn parse_vscode_tokens(raw: &str) -> Result<Option<NativeToken>> {
    let document: Value =
        serde_json::from_str(raw).context("VS Code MCP token record is not valid JSON")?;
    let entries = document
        .as_array()
        .ok_or_else(|| anyhow!("VS Code MCP token record is not an array"))?;
    if entries.is_empty() {
        return Ok(None);
    }

    let mut valid: Option<(u64, NativeToken)> = None;
    let mut expired: Option<(u64, NativeToken)> = None;
    let mut malformed = None;
    for entry in entries {
        let Some(entry) = entry.as_object() else {
            if malformed.is_none() {
                malformed = Some(anyhow!(
                    "VS Code MCP token record contains a non-object entry"
                ));
            }
            continue;
        };
        let Some(access_token) =
            string_field(entry, &["access_token"]).filter(|token| !token.is_empty())
        else {
            if malformed.is_none() {
                malformed = Some(anyhow!(
                    "VS Code MCP token record contains an entry without an access token"
                ));
            }
            continue;
        };

        let created_at =
            optional_number_field(entry, &["created_at"], "VS Code token creation time")?;
        let expires_in = optional_number_field(entry, &["expires_in"], "VS Code token lifetime")?;
        let expires_at = created_at.and_then(|created_at| {
            expires_in.map(|expires_in| {
                normalize_timestamp(created_at.saturating_add(expires_in.saturating_mul(1000)))
            })
        });

        let token = NativeToken {
            access_token: access_token.to_string(),
            expires_at,
            source: "VS Code",
        };
        let rank = created_at.unwrap_or(expires_at.unwrap_or(0));
        let bucket = if token_is_expired(token.expires_at) {
            &mut expired
        } else {
            &mut valid
        };
        if bucket.as_ref().is_none_or(|(best, _)| rank >= *best) {
            *bucket = Some((rank, token));
        }
    }

    if let Some((_, token)) = valid {
        return Ok(Some(token));
    }
    if let Some((_, token)) = expired {
        return Ok(Some(token));
    }
    if let Some(error) = malformed {
        return Err(error);
    }
    Ok(None)
}

const CURSOR_SAFE_STORAGE_SERVICE: &str = "Cursor Safe Storage";
const CURSOR_SAFE_STORAGE_ACCOUNT: &str = "Cursor";

enum CursorScope {
    Url(String),
    /// Bound through the sibling `mcp_server_url` global-state entry.
    Identifier(String),
}

fn cursor_secret_parts(key: &str) -> Option<(String, CursorScope)> {
    let encoded = key.strip_prefix("mcpOAuth.secret.")?;
    let decoded = decode_base64url(encoded)?;
    let inner = String::from_utf8(decoded).ok()?;

    let (prefix, rest) = if let Some(after) = inner.strip_prefix("[profile:") {
        let end = after.find("] ")?;
        let prefix_end = "[profile:".len() + end + 2;
        (&inner[..prefix_end], &inner[prefix_end..])
    } else {
        ("", inner.as_str())
    };

    let payload = rest.strip_prefix('[')?;
    let end = payload.find("] ")?;
    let tag = &payload[..end];
    if payload.get(end + 2..) != Some("mcp_tokens") || tag.is_empty() {
        return None;
    }
    let scope = if let Some(encoded_url) = tag.strip_prefix("url:") {
        CursorScope::Url(String::from_utf8(decode_base64url(encoded_url)?).ok()?)
    } else if tag.starts_with("oauth-attempt:") {
        return None;
    } else {
        CursorScope::Identifier(tag.to_string())
    };
    Some((prefix.to_string(), scope))
}

pub(crate) fn cursor_secret_key_matches(key: &str, url: &str) -> bool {
    cursor_secret_parts(key).is_some_and(|(_, scope)| match scope {
        CursorScope::Url(stored_url) => urls_match(&stored_url, url),
        CursorScope::Identifier(_) => false,
    })
}

pub(crate) fn cursor_updated_at_storage_key(secret_key: &str) -> Option<String> {
    let (prefix, scope) = cursor_secret_parts(secret_key)?;
    let tag = match scope {
        CursorScope::Url(url) => {
            format!(
                "url:{}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(url)
            )
        }
        CursorScope::Identifier(identifier) => identifier,
    };
    let inner = format!("{prefix}[{tag}] mcp_oauth_updated_at_ms");
    Some(format!(
        "mcpOAuth.global.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(inner)
    ))
}

pub(crate) fn cursor_server_url_storage_key(secret_key: &str) -> Option<String> {
    let (prefix, scope) = cursor_secret_parts(secret_key)?;
    let CursorScope::Identifier(identifier) = scope else {
        return None;
    };
    let inner = format!("{prefix}[{identifier}] mcp_server_url");
    Some(format!(
        "mcpOAuth.global.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(inner)
    ))
}

pub(crate) fn parse_cursor_secret_value(
    value: &[u8],
    safe_storage_password: &str,
    updated_at_ms: Option<u64>,
) -> Result<Option<NativeToken>> {
    let ciphertext = electron_buffer_data(value)?;
    let plaintext = decrypt_electron_safe_storage(&ciphertext, safe_storage_password)?;
    parse_cursor_token(&plaintext, updated_at_ms)
}

fn parse_cursor_token(raw: &str, updated_at_ms: Option<u64>) -> Result<Option<NativeToken>> {
    let document: Value =
        serde_json::from_str(raw).context("Cursor MCP token record is not valid JSON")?;
    let entry = document
        .as_object()
        .ok_or_else(|| anyhow!("Cursor MCP token record is not a JSON object"))?;
    let access_token = string_field(entry, &["access_token"])
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow!("Cursor MCP token record has no access token"))?;
    string_field(entry, &["token_type"])
        .filter(|token_type| !token_type.is_empty())
        .ok_or_else(|| anyhow!("Cursor MCP token record has no token type"))?;

    let explicit_expiry =
        optional_timestamp_field(entry, &["expires_at", "expiresAt"], "Cursor token")?;
    let expires_at = match explicit_expiry {
        Some(expiry) => Some(expiry),
        None => {
            let expires_in =
                optional_number_field(entry, &["expires_in"], "Cursor token lifetime")?;
            match (expires_in, updated_at_ms) {
                (Some(expires_in), Some(updated_at_ms)) => Some(normalize_timestamp(
                    updated_at_ms.saturating_add(expires_in.saturating_mul(1000)),
                )),
                _ => None,
            }
        }
    };

    Ok(Some(NativeToken {
        access_token: access_token.to_string(),
        expires_at,
        source: "Cursor",
    }))
}

fn decode_base64url(value: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(value))
        .ok()
}

#[cfg(target_os = "macos")]
fn claude_keychain_lookup(
    _server_name: &str,
    url: &str,
    env: &BTreeMap<String, String>,
) -> Result<Option<NativeToken>> {
    let service = claude_service_name(env);
    let account = claude_account(env);
    let Some(raw) = keychain_password(&service, &account, "Claude Code")? else {
        return Ok(None);
    };
    parse_claude_store(&raw, url).context("Claude Code credential store is malformed")
}

#[cfg(target_os = "macos")]
fn codex_keychain_lookup(
    server_name: &str,
    url: &str,
    _env: &BTreeMap<String, String>,
) -> Result<Option<NativeToken>> {
    let Some(keys) = codex_account_keys(server_name, url) else {
        return Ok(None);
    };
    for account in keys {
        if let Some(raw) = keychain_password(CODEX_SERVICE, &account, "Codex")? {
            return parse_codex_store(&raw, url).context("Codex credential record is malformed");
        }
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
fn vscode_lookup(
    _server_name: &str,
    url: &str,
    env: &BTreeMap<String, String>,
) -> Result<Option<NativeToken>> {
    let mut safe_storage_password = None;
    let mut matching_error = None;
    let mut database_error = None;
    let mut expired = None;

    for path in vscode_state_paths(env) {
        if !path.is_file() {
            continue;
        }
        let rows = match read_storage_rows(&path, "secret://", "VS Code") {
            Ok(rows) => rows,
            Err(error) => {
                if database_error.is_none() {
                    database_error = Some(error);
                }
                continue;
            }
        };

        for (key, value) in rows {
            if !vscode_secret_key_matches(&key, url) {
                continue;
            }

            if safe_storage_password.is_none() {
                match keychain_password(
                    VSCODE_SAFE_STORAGE_SERVICE,
                    VSCODE_SAFE_STORAGE_ACCOUNT,
                    "VS Code",
                )? {
                    Some(password) => safe_storage_password = Some(password),
                    None => {
                        if matching_error.is_none() {
                            matching_error = Some(anyhow!(
                                "VS Code has a stored MCP credential for the requested endpoint, \
                                 but its macOS Safe Storage key is missing; open VS Code and retry"
                            ));
                        }
                        continue;
                    }
                }
            }

            let password = safe_storage_password
                .as_deref()
                .expect("safe storage password was initialized above");
            match parse_vscode_secret_value(&value, password) {
                Ok(Some(token)) if token_is_expired(token.expires_at) => {
                    if expired.is_none() {
                        expired = Some(token);
                    }
                }
                Ok(Some(token)) => return Ok(Some(token)),
                Ok(None) => {}
                Err(error) => {
                    if matching_error.is_none() {
                        matching_error =
                            Some(error.context("VS Code stored MCP credential could not be read"));
                    }
                }
            }
        }
    }

    if let Some(token) = expired {
        return Ok(Some(token));
    }
    if let Some(error) = matching_error {
        return Err(error);
    }
    if let Some(error) = database_error {
        return Err(error);
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
fn cursor_lookup(
    _server_name: &str,
    url: &str,
    env: &BTreeMap<String, String>,
) -> Result<Option<NativeToken>> {
    let Some(path) = cursor_state_path(env) else {
        return Ok(None);
    };
    if !path.is_file() {
        return Ok(None);
    }

    let rows = read_storage_rows(&path, "mcpOAuth.", "Cursor")?;
    let mut globals = BTreeMap::new();
    let mut secrets = Vec::new();
    for (key, value) in rows {
        if key.starts_with("mcpOAuth.global.") {
            globals.insert(key, value);
        } else if let Some((prefix, scope)) = cursor_secret_parts(&key) {
            if matches!(&scope, CursorScope::Url(_)) && !cursor_secret_key_matches(&key, url) {
                continue;
            }
            secrets.push((prefix.is_empty(), key, value, scope));
        }
    }
    if secrets.is_empty() {
        return Ok(None);
    }
    // Prefer the default profile over profile-scoped copies of the same grant.
    secrets.sort_by(|(a_default, a_key, _, _), (b_default, b_key, _, _)| {
        b_default.cmp(a_default).then_with(|| a_key.cmp(b_key))
    });

    let mut safe_storage_password = None;
    let mut matching_error = None;
    let mut expired = None;
    for (_is_default, key, value, scope) in secrets {
        if matches!(scope, CursorScope::Identifier(_)) {
            let Some(server_url) = cursor_server_url_storage_key(&key)
                .and_then(|global_key| globals.get(&global_key))
                .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
                .and_then(|value| value.as_str().map(str::to_owned))
            else {
                continue;
            };
            if !urls_match(&server_url, url) {
                continue;
            }
        }

        let updated_at_ms = cursor_updated_at_storage_key(&key)
            .and_then(|global_key| globals.get(&global_key))
            .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
            .and_then(|value| json_u64(&value).ok());

        if safe_storage_password.is_none() {
            match keychain_password(
                CURSOR_SAFE_STORAGE_SERVICE,
                CURSOR_SAFE_STORAGE_ACCOUNT,
                "Cursor",
            )? {
                Some(password) => safe_storage_password = Some(password),
                None => {
                    if matching_error.is_none() {
                        matching_error = Some(anyhow!(
                            "Cursor has a stored MCP credential for the requested endpoint, \
                             but its macOS Safe Storage key is missing; open Cursor and retry"
                        ));
                    }
                    continue;
                }
            }
        }

        let password = safe_storage_password
            .as_deref()
            .expect("safe storage password was initialized above");
        match parse_cursor_secret_value(&value, password, updated_at_ms) {
            Ok(Some(token)) if token_is_expired(token.expires_at) => {
                if expired.is_none() {
                    expired = Some(token);
                }
            }
            Ok(Some(token)) => return Ok(Some(token)),
            Ok(None) => {}
            Err(error) => {
                if matching_error.is_none() {
                    matching_error =
                        Some(error.context("Cursor stored MCP credential could not be read"));
                }
            }
        }
    }

    if let Some(token) = expired {
        return Ok(Some(token));
    }
    if let Some(error) = matching_error {
        return Err(error);
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
fn cursor_state_path(env: &BTreeMap<String, String>) -> Option<std::path::PathBuf> {
    let home = env.get("HOME").filter(|value| !value.is_empty())?;
    Some(
        std::path::Path::new(home)
            .join("Library")
            .join("Application Support")
            .join("Cursor")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb"),
    )
}

#[cfg(target_os = "macos")]
fn vscode_state_paths(env: &BTreeMap<String, String>) -> Vec<std::path::PathBuf> {
    let Some(home) = env.get("HOME").filter(|value| !value.is_empty()) else {
        return Vec::new();
    };
    let user_dir = std::path::Path::new(home)
        .join("Library")
        .join("Application Support")
        .join("Code")
        .join("User");

    let mut paths = vec![user_dir.join("globalStorage").join("state.vscdb")];
    let profiles = user_dir.join("profiles");
    if let Ok(entries) = std::fs::read_dir(&profiles) {
        let mut profile_paths = entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path().join("globalStorage").join("state.vscdb"))
            .collect::<Vec<_>>();
        profile_paths.sort();
        paths.append(&mut profile_paths);
    }
    paths
}

#[cfg(target_os = "macos")]
fn read_storage_rows(
    path: &std::path::Path,
    prefix: &str,
    label: &str,
) -> Result<Vec<(String, Vec<u8>)>> {
    use rusqlite::{Connection, OpenFlags, params, types::ValueRef};

    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| {
        format!(
            "cannot open the {label} credential database at {}",
            path.display()
        )
    })?;
    let _ = connection.busy_timeout(std::time::Duration::from_millis(500));

    let mut statement = connection
        .prepare("SELECT key, value FROM ItemTable WHERE key LIKE ?1")
        .with_context(|| {
            format!(
                "{label} credential database at {} has no readable ItemTable",
                path.display()
            )
        })?;
    let mut rows = statement
        .query(params![format!("{prefix}%")])
        .with_context(|| {
            format!(
                "cannot query the {label} credential database at {}",
                path.display()
            )
        })?;

    let mut result = Vec::new();
    while let Some(row) = rows.next().with_context(|| {
        format!(
            "cannot read the {label} credential database at {}",
            path.display()
        )
    })? {
        let key = match row.get_ref(0) {
            Ok(ValueRef::Text(bytes)) | Ok(ValueRef::Blob(bytes)) => {
                String::from_utf8(bytes.to_vec()).ok()
            }
            Ok(_) | Err(_) => None,
        };
        let Some(key) = key else {
            continue;
        };
        let value = match row.get_ref(1) {
            Ok(ValueRef::Text(bytes)) | Ok(ValueRef::Blob(bytes)) => Some(bytes.to_vec()),
            Ok(_) | Err(_) => None,
        };
        if let Some(value) = value {
            result.push((key, value));
        }
    }
    Ok(result)
}

#[cfg(target_os = "macos")]
fn keychain_password(service: &str, account: &str, label: &str) -> Result<Option<String>> {
    use keyring::{Entry, Error};

    let entry = Entry::new(service, account)
        .map_err(|error| anyhow!("cannot open the {label} macOS Keychain entry: {error}"))?;
    match entry.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(Error::NoEntry) => Ok(None),
        Err(Error::NoStorageAccess(error)) => Err(anyhow!(
            "the {label} credential store is locked or inaccessible ({error}); \
             unlock the macOS login keychain and allow cm access, then retry"
        )),
        Err(Error::PlatformFailure(error)) => Err(anyhow!(
            "the {label} credential store could not be read from the macOS Keychain ({error}); \
             unlock the login keychain and allow cm access, then retry"
        )),
        Err(error) => Err(anyhow!("cannot read the {label} credential store: {error}")),
    }
}

fn string_field<'a>(object: &'a Map<String, Value>, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(Value::as_str))
}

fn optional_number_field(
    object: &Map<String, Value>,
    names: &[&str],
    what: &str,
) -> Result<Option<u64>> {
    let Some(value) = names.iter().find_map(|name| object.get(*name)) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    json_u64(value)
        .with_context(|| format!("{what} is malformed"))
        .map(Some)
}

fn optional_timestamp_field(
    object: &Map<String, Value>,
    names: &[&str],
    what: &str,
) -> Result<Option<u64>> {
    Ok(optional_number_field(object, names, what)?.map(normalize_timestamp))
}

fn json_u64(value: &Value) -> Result<u64> {
    if let Some(number) = value.as_u64() {
        return Ok(number);
    }
    if let Some(text) = value.as_str() {
        return text
            .parse::<u64>()
            .map_err(|_| anyhow!("expected a non-negative integer"));
    }
    if let Some(number) = value.as_f64()
        && number.is_finite()
        && number >= 0.0
        && number.fract() == 0.0
        && number <= u64::MAX as f64
    {
        return Ok(number as u64);
    }
    bail!("expected a non-negative integer")
}

fn normalize_timestamp(value: u64) -> u64 {
    if value > 10_000_000_000 {
        value / 1000
    } else {
        value
    }
}

fn token_is_expired(expires_at: Option<u64>) -> bool {
    expires_at.is_some_and(|expires_at| expires_at <= now_seconds().saturating_add(30))
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn urls_match(left: &str, right: &str) -> bool {
    match (reqwest::Url::parse(left), reqwest::Url::parse(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn hash_prefix(value: &str, bytes: usize) -> String {
    let digest = Sha256::digest(value.as_bytes());
    hex::encode(&digest[..bytes])
}

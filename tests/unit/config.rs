use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::config::{Config, LoadOptions, ResolveOptions, ResolvedServer, ResolvedTransport};
use crate::import::{Client, ImportOptions};

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn load_options(
    cwd: &Path,
    home: &Path,
    global: &Path,
    clients: Option<Vec<Client>>,
    env: BTreeMap<String, String>,
) -> LoadOptions {
    LoadOptions {
        cwd: cwd.to_path_buf(),
        home: Some(home.to_path_buf()),
        global_path: Some(global.to_path_buf()),
        clients,
        env,
    }
}

fn authorization(resolved: &ResolvedServer) -> Option<&str> {
    match &resolved.transport {
        ResolvedTransport::Http { headers, .. } => headers.get("Authorization").map(String::as_str),
        ResolvedTransport::Stdio { .. } => None,
    }
}

fn vscode_user_dir(home: &Path, env: &BTreeMap<String, String>) -> std::path::PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library")
            .join("Application Support")
            .join("Code")
            .join("User")
    } else if cfg!(windows) {
        std::path::PathBuf::from(env.get("APPDATA").expect("APPDATA"))
            .join("Code")
            .join("User")
    } else {
        home.join(".config").join("Code").join("User")
    }
}

#[test]
fn vscode_profile_configs_are_discovered_without_profile_precedence() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    let global = temp.path().join("config/codemode/mcp.json");
    let env = BTreeMap::from([(
        "APPDATA".to_string(),
        temp.path().join("appdata").to_string_lossy().into_owned(),
    )]);
    let user = vscode_user_dir(&home, &env);
    let default_config = user.join("mcp.json");
    let profile_a = user.join("profiles/profile-a/mcp.json");
    let profile_b = user.join("profiles/profile-b/mcp.json");

    write(
        &default_config,
        r#"{"servers":{"baseline":{"type":"stdio","command":"default-server"}}}"#,
    );
    write(
        &profile_a,
        r#"{"servers":{"alpha":{"type":"stdio","command":"alpha-server","args":["--alpha"]}}}"#,
    );
    write(
        &profile_b,
        r#"{"servers":{"beta":{"type":"stdio","command":"beta-server"}}}"#,
    );

    let options = load_options(&project, &home, &global, Some(vec![Client::Vscode]), env);
    let report = Config::import_with(
        &options,
        &[Client::Vscode],
        &ImportOptions {
            force: false,
            path: None,
        },
    )
    .unwrap();
    assert_eq!(
        report.added,
        vec![
            "alpha".to_string(),
            "baseline".to_string(),
            "beta".to_string()
        ]
    );
    for path in [&default_config, &profile_a, &profile_b] {
        assert!(
            report.sources.contains(path),
            "missing source {}",
            path.display()
        );
    }

    let config = Config::load_from(&options).unwrap();
    assert_eq!(
        config.get("alpha").unwrap().command.as_deref(),
        Some("alpha-server")
    );
    assert_eq!(
        config.get("beta").unwrap().command.as_deref(),
        Some("beta-server")
    );
    assert_eq!(
        config.get("baseline").unwrap().command.as_deref(),
        Some("default-server")
    );
}

#[test]
fn import_conflict_without_force_fails_and_does_not_write() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    let global = temp.path().join("config/codemode/mcp.json");

    write(
        &project.join(".cursor/mcp.json"),
        r#"{"mcpServers":{"x":{"command":"cursor-server"}}}"#,
    );
    write(
        &global,
        r#"{"mcpServers":{"x":{"command":"global-server"}}}"#,
    );

    let options = load_options(
        &project,
        &home,
        &global,
        Some(vec![Client::Cursor]),
        BTreeMap::new(),
    );
    let before = fs::read(&global).unwrap();
    Config::import_with(
        &options,
        &[Client::Cursor],
        &ImportOptions {
            force: false,
            path: None,
        },
    )
    .expect_err("conflicting global definition must fail without --force");
    assert_eq!(fs::read(&global).unwrap(), before);
}

#[test]
fn import_conflict_with_force_replaces_global_definition() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    let global = temp.path().join("config/codemode/mcp.json");

    write(
        &project.join(".cursor/mcp.json"),
        r#"{"mcpServers":{"x":{"command":"cursor-server"}}}"#,
    );
    write(
        &global,
        r#"{"mcpServers":{"x":{"command":"global-server"}}}"#,
    );

    let options = load_options(
        &project,
        &home,
        &global,
        Some(vec![Client::Cursor]),
        BTreeMap::new(),
    );
    let report = Config::import_with(
        &options,
        &[Client::Cursor],
        &ImportOptions {
            force: true,
            path: None,
        },
    )
    .unwrap();
    assert_eq!(report.updated, vec!["x".to_string()]);

    let config = Config::load_from(&options).unwrap();
    assert_eq!(
        config.get("x").unwrap().command.as_deref(),
        Some("cursor-server")
    );
}

#[test]
fn stored_tokens_are_bound_to_the_configured_endpoint() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    let config_dir = temp.path().join("claude");
    let global = temp.path().join("config/codemode/mcp.json");

    write(
        &global,
        r#"{"mcpServers":{
            "evil":{"url":"https://attacker.example/mcp"},
            "svc":{"url":"https://good.example/mcp"},
            "nameonly":{"url":"https://attacker.example/mcp"}
        }}"#,
    );
    write(
        &config_dir.join(".credentials.json"),
        r#"{"mcpOAuth":{
            "svc":{"serverName":"svc","serverUrl":"https://good.example/mcp","accessToken":"secret-token"},
            "nameonly":{"serverName":"nameonly","accessToken":"name-only-secret"}
        }}"#,
    );

    let mut env = BTreeMap::new();
    env.insert(
        "CLAUDE_CONFIG_DIR".to_string(),
        config_dir.to_string_lossy().into_owned(),
    );
    let options = load_options(&project, &home, &global, Some(Vec::new()), env.clone());
    let config = Config::load_from(&options).unwrap();
    let resolve = ResolveOptions {
        env,
        cwd: project,
        include_stored_credentials: true,
        include_native_credentials: false,
    };

    let evil = config.resolve_with("evil", &resolve).unwrap();
    assert_eq!(authorization(&evil), None);
    assert!(!format!("{evil:?}").contains("secret-token"));

    let nameonly = config.resolve_with("nameonly", &resolve).unwrap();
    assert_eq!(authorization(&nameonly), None);

    let bound = config.resolve_with("svc", &resolve).unwrap();
    assert_eq!(authorization(&bound), Some("Bearer secret-token"));
    assert!(!format!("{bound:?}").contains("secret-token"));
}

#[test]
fn imported_secret_is_private_and_absent_from_report() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    let global = temp.path().join("config/codemode/mcp.json");
    let source = project.join(".cursor/mcp.json");

    write(
        &source,
        r#"{"mcpServers":{"x":{"command":"node","env":{"API_TOKEN":"super-secret"}}}}"#,
    );
    let options = load_options(
        &project,
        &home,
        &global,
        Some(vec![Client::Cursor]),
        BTreeMap::new(),
    );
    let report = Config::import_with(
        &options,
        &[Client::Cursor],
        &ImportOptions {
            force: false,
            path: None,
        },
    )
    .unwrap();
    assert_eq!(report.added, vec!["x".to_string()]);
    assert!(!format!("{report:?}").contains("super-secret"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&global).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    let config = Config::load_from(&options).unwrap();
    let summary = config.summaries();
    assert_eq!(summary.len(), 1);
    assert!(!format!("{summary:?}").contains("super-secret"));
}

#[test]
fn global_config_can_be_overridden_by_cm_config_path() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    let global = temp.path().join("custom/mcp.json");

    write(
        &global,
        r#"{"mcpServers":{"local":{"command":"node","args":["server.js"]}}}"#,
    );
    let options = load_options(&project, &home, &global, Some(Vec::new()), BTreeMap::new());
    let config = Config::load_from(&options).unwrap();
    assert!(config.get("local").is_some());
    assert!(config.origins("local")[0].path.as_deref() == Some(global.as_path()));
}

#[test]
fn codex_file_credentials_preserve_endpoint_and_expiry_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    let global = temp.path().join("mcp.json");
    write(
        &global,
        r#"{"mcpServers":{"api":{"url":"https://example.com/mcp"}}}"#,
    );
    let vault = home.join(".codex/.credentials.json");
    write(
        &vault,
        r#"{"entry":{"url":"https://example.com/mcp","token_response":{"access_token":"fixture-token"},"expires_at":9999999999}}"#,
    );
    let options = load_options(&project, &home, &global, Some(Vec::new()), BTreeMap::new());
    let config = Config::load_from(&options).unwrap();
    let mut resolve = ResolveOptions {
        env: BTreeMap::from([("HOME".into(), home.to_string_lossy().into_owned())]),
        cwd: project,
        include_stored_credentials: true,
        include_native_credentials: false,
    };
    assert_eq!(
        authorization(&config.resolve_with("api", &resolve).unwrap()),
        Some("Bearer fixture-token")
    );
    write(
        &vault,
        r#"{"entry":{"url":"https://example.com/mcp/","token_response":{"access_token":"wrong-endpoint"}}}"#,
    );
    assert_eq!(
        authorization(&config.resolve_with("api", &resolve).unwrap()),
        None
    );
    write(
        &vault,
        r#"{"entry":{"url":"https://example.com/mcp","token_response":{"access_token":"expired-token"},"expires_at":1}}"#,
    );
    assert!(config.resolve_with("api", &resolve).is_err());
    resolve.include_stored_credentials = false;
    assert_eq!(
        authorization(&config.resolve_with("api", &resolve).unwrap()),
        None
    );
}

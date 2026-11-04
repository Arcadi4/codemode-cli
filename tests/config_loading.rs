use std::{
    fs,
    process::{Command, Output},
};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Workspace(TempDir);

impl Workspace {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    fn write(&self, path: &str, contents: &str) {
        let path = self.0.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn global(&self) -> std::path::PathBuf {
        self.0.path().join("global.json")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cm"))
            .env_clear()
            .env("HOME", self.0.path())
            .env("XDG_CONFIG_HOME", self.0.path().join(".config"))
            .env("CM_CONFIG", self.global())
            .current_dir(self.0.path())
            .args(args)
            .output()
            .unwrap()
    }
}

fn successful_json(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn discovery_keeps_valid_sources_when_other_configs_are_invalid_or_missing() {
    let workspace = Workspace::new();
    workspace.write("global.json", "{broken");
    workspace.write(".cursor/mcp.json", "{broken");
    workspace.write(
        ".codex/config.toml",
        "[mcp_servers.working]\ncommand = 'fixture-server'\n",
    );
    workspace.write(
        ".mcp.json",
        r#"{"mcpServers":{
            "invalid":{"args":[]},
            "bad-shape":{"command":42},
            "copilot-good":{"command":"fixture-server"}
        }}"#,
    );

    let output = workspace.run(&["servers", "list"]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(stderr.contains("bad-shape"), "{stderr}");
    let servers = successful_json(output);
    assert_eq!(
        servers
            .as_array()
            .unwrap()
            .iter()
            .map(|server| server["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["copilot-good", "working"]
    );
    assert_eq!(fs::read_to_string(workspace.global()).unwrap(), "{broken");
}

#[test]
fn invalid_global_entries_warn_but_import_never_overwrites_them() {
    let workspace = Workspace::new();
    let original = r#"{"mcpServers":{
        "bad":{"command":42},
        "existing":{"command":"existing-server"}
    }}"#;
    workspace.write("global.json", original);

    let output = workspace.run(&["servers", "list"]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(stderr.contains("bad"), "{stderr}");
    let servers = successful_json(output);
    assert_eq!(
        servers
            .as_array()
            .unwrap()
            .iter()
            .map(|server| server["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["existing"]
    );

    workspace.write(
        ".cursor/mcp.json",
        r#"{"mcpServers":{"added":{"command":"cursor-server"}}}"#,
    );
    let output = workspace.run(&["import", "cursor", "--force"]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!stderr.is_empty(), "invalid destination must be reported");
    let report = successful_json(output);
    assert_eq!(report["added"], json!([]));
    assert_eq!(fs::read_to_string(workspace.global()).unwrap(), original);
}

#[test]
fn invalid_and_missing_import_sources_leave_global_config_unchanged() {
    let workspace = Workspace::new();
    let original = r#"{"mcpServers":{"existing":{"command":"existing-server"}}}"#;
    workspace.write("global.json", original);
    for (name, contents) in [
        ("malformed.json", "{broken"),
        ("invalid.json", r#"{"mcpServers":[]}"#),
        (
            "incomplete.json",
            r#"{"mcpServers":{"invalid":{"args":[]}}}"#,
        ),
    ] {
        workspace.write(name, contents);
    }
    for path in [
        "missing.json",
        "malformed.json",
        "invalid.json",
        "incomplete.json",
    ] {
        let report = successful_json(workspace.run(&["import", "cursor", "--path", path]));
        assert_eq!(report["added"], json!([]));
        assert_eq!(report["updated"], json!([]));
        assert_eq!(fs::read_to_string(workspace.global()).unwrap(), original);
    }
    let report = successful_json(workspace.run(&["import", "cursor"]));
    assert_eq!(report["added"], json!([]));
    assert_eq!(fs::read_to_string(workspace.global()).unwrap(), original);
}

#[test]
fn import_all_skips_bad_sources_without_overwriting_invalid_destination() {
    let workspace = Workspace::new();
    workspace.write(".cursor/mcp.json", "{broken");
    workspace.write(
        ".codex/config.toml",
        "[mcp_servers.working]\ncommand = 'fixture-server'\n",
    );
    let report = successful_json(workspace.run(&["import", "--all"]));
    assert_eq!(report["added"], json!(["working"]));
    let global: Value = serde_json::from_slice(&fs::read(workspace.global()).unwrap()).unwrap();
    assert_eq!(global["mcpServers"]["working"]["command"], "fixture-server");

    workspace.write("global.json", "{broken");
    let report = successful_json(workspace.run(&["import", "--all", "--force"]));
    assert_eq!(report["added"], json!([]));
    assert_eq!(fs::read_to_string(workspace.global()).unwrap(), "{broken");
}

#[test]
fn absent_configurations_are_silent_but_existing_invalid_files_warn() {
    let workspace = Workspace::new();
    let missing = "Library/Application Support/Code/User/mcp.json";
    for arguments in [
        vec!["servers", "list"],
        vec!["import", "vscode"],
        vec!["import", "vscode", "--path", missing],
    ] {
        let output = workspace.run(&arguments);
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result = successful_json(output);
        if arguments[0] == "servers" {
            assert_eq!(result, json!([]));
        } else {
            assert_eq!(result["added"], json!([]));
        }
    }
    workspace.write(missing, "{invalid");
    let output = workspace.run(&["import", "vscode", "--path", missing]);
    assert!(!output.stderr.is_empty());
    assert_eq!(successful_json(output)["added"], json!([]));
    assert!(!workspace.global().exists());
}

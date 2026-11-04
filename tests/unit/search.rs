use clap::Parser;
use serde_json::{Value, json};

use crate::host::CatalogEntry;
use crate::search::{Field, SearchArgs, search};

fn entry(
    server: &str,
    native: &str,
    title: Option<&str>,
    description: Option<&str>,
) -> CatalogEntry {
    let mut definition = json!({"name": native});
    if let Some(title) = title {
        definition["title"] = json!(title);
    }
    if let Some(description) = description {
        definition["description"] = json!(description);
    }
    CatalogEntry {
        name: format!("{server}_{native}"),
        server: server.to_owned(),
        definition,
    }
}

fn args(query: &str) -> SearchArgs {
    SearchArgs {
        query: query.to_owned(),
        limit: 20,
        fields: vec![Field::Name, Field::Server, Field::Description],
    }
}

fn names(result: &Value) -> Vec<String> {
    result
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value["name"].as_str().unwrap().to_owned())
        .collect()
}

fn metadata_fixtures() -> Vec<CatalogEntry> {
    vec![
        entry(
            "fs",
            "read_file",
            Some("Read a file"),
            Some("Reads text from disk"),
        ),
        entry(
            "fs",
            "read_dir",
            Some("List a directory"),
            Some("Lists files and folders"),
        ),
        entry(
            "fs",
            "write_file",
            Some("Write a file"),
            Some("Writes text to disk"),
        ),
        entry(
            "fs",
            "fetch",
            Some("Retrieve"),
            Some("Downloads a resource"),
        ),
        entry("fs", "über_tool", Some("Über Tool"), None),
    ]
}

#[test]
fn every_term_must_match_case_insensitively_across_metadata() {
    assert_eq!(
        names(&search(metadata_fixtures(), &args("ReAd FiLe"))),
        ["fs_read_file", "fs_read_dir"]
    );
    assert_eq!(
        names(&search(metadata_fixtures(), &args("directory"))),
        ["fs_read_dir"]
    );
    assert_eq!(
        names(&search(metadata_fixtures(), &args("DISK"))),
        ["fs_read_file", "fs_write_file"]
    );
    assert_eq!(
        names(&search(metadata_fixtures(), &args("fetch"))),
        ["fs_fetch"]
    );
    assert_eq!(
        names(&search(metadata_fixtures(), &args("ÜBER"))),
        ["fs_über_tool"]
    );
}

fn ranking_fixtures() -> Vec<CatalogEntry> {
    vec![
        entry("s", "thread_read", None, None),
        entry("s", "reader", None, None),
        entry("s", "other", None, Some("read helper")),
        entry("s", "read", None, None),
        entry("a", "read", None, None),
    ]
}

#[test]
fn ranking_orders_name_matches_before_metadata_and_applies_limit() {
    assert_eq!(
        names(&search(ranking_fixtures(), &args("read"))),
        ["a_read", "s_read", "s_reader", "s_thread_read", "s_other"]
    );
    let mut limited = args("read");
    limited.limit = 2;
    assert_eq!(
        names(&search(ranking_fixtures(), &limited)),
        ["a_read", "s_read"]
    );
}

#[test]
fn equal_rank_and_name_ties_break_by_server() {
    let entries = vec![
        CatalogEntry {
            name: "a_b_c_read".to_owned(),
            server: "a_b_c".to_owned(),
            definition: json!({"name": "read"}),
        },
        CatalogEntry {
            name: "a_b_c_read".to_owned(),
            server: "a".to_owned(),
            definition: json!({"name": "b_c_read"}),
        },
        CatalogEntry {
            name: "a_b_c_read".to_owned(),
            server: "a_b".to_owned(),
            definition: json!({"name": "c_read"}),
        },
    ];
    let result = search(entries, &args("a_b_c"));
    let servers: Vec<_> = result
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value["server"].as_str().unwrap())
        .collect();
    assert_eq!(servers, ["a", "a_b", "a_b_c"]);
}

fn projection_entry() -> CatalogEntry {
    let mut entry = entry("srv", "read_file", Some("Read File"), Some("Reads a file"));
    entry.definition["inputSchema"] = json!({"type": "object"});
    entry.definition["outputSchema"] = json!({"type": "object"});
    entry
}

#[test]
fn projection_selects_summary_fields_and_omits_missing_or_schema_data() {
    assert_eq!(
        search(vec![projection_entry()], &args("read")),
        json!([{
            "name": "srv_read_file",
            "server": "srv",
            "description": "Reads a file"
        }])
    );

    let mut all_fields = args("read");
    all_fields.fields = vec![Field::Name, Field::Server, Field::Title, Field::Description];
    assert_eq!(
        search(vec![projection_entry()], &all_fields),
        json!([{
            "name": "srv_read_file",
            "server": "srv",
            "title": "Read File",
            "description": "Reads a file"
        }])
    );
    assert_eq!(
        search(vec![entry("srv", "read_file", None, None)], &all_fields),
        json!([{"name": "srv_read_file", "server": "srv"}])
    );

    let mut name_only = args("read");
    name_only.fields = vec![Field::Name];
    assert_eq!(
        search(vec![projection_entry()], &name_only),
        json!([{"name": "srv_read_file"}])
    );
}

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    args: SearchArgs,
}

#[test]
fn clap_rejects_empty_query_nonpositive_limit_and_unknown_fields() {
    assert!(Cli::try_parse_from(["search", ""]).is_err());
    assert!(Cli::try_parse_from(["search", " \t\n"]).is_err());
    assert!(Cli::try_parse_from(["search", "read", "--limit", "0"]).is_err());
    assert!(Cli::try_parse_from(["search", "read", "--fields", "name,bogus"]).is_err());
}

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, prelude::BASE64_STANDARD};
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::Value;

pub fn validate_schema(schema: &Value) -> Result<()> {
    ensure!(
        schema.is_object(),
        "MCP tool schemas must be JSON Schema objects"
    );
    bounds(schema, 0, &mut 0)?;
    jsonschema::meta::validate(schema)
        .map_err(|error| anyhow::anyhow!("invalid JSON Schema: {error}"))
}

pub fn validate_instance(schema: &Value, instance: &Value, label: &str) -> Result<()> {
    bounds(schema, 0, &mut 0)?;
    // Network and file retrieval features are disabled. External references fail closed.
    let mut options = jsonschema::options();
    if schema.get("$schema").is_none() {
        options = options.with_draft(jsonschema::Draft::Draft202012);
    }
    let validator = options
        .build(schema)
        .context("cannot compile JSON Schema (external references are not fetched)")?;
    if let Err(error) = validator.validate(instance) {
        // Do not echo argument values: they can contain credentials or private resource data.
        bail!(
            "{label} does not match the schema at {} ({})",
            error.instance_path(),
            error.schema_path()
        );
    }
    Ok(())
}

fn bounds(value: &Value, depth: usize, nodes: &mut usize) -> Result<()> {
    *nodes += 1;
    ensure!(
        depth <= 64 && *nodes <= 20_000,
        "JSON Schema exceeds complexity limits"
    );
    match value {
        Value::Object(map) => {
            for child in map.values() {
                bounds(child, depth + 1, nodes)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                bounds(child, depth + 1, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

// rmcp owns transport framing and SSE parsing, but its SEP-2243 projection helpers
// are crate-private. This small projection also works when discovery came from disk.
pub fn validate_annotations(schema: &Value) -> Result<()> {
    let mut seen = HashSet::new();
    annotations(schema, false, true, &mut seen)
}

fn annotations(
    schema: &Value,
    property: bool,
    reachable: bool,
    seen: &mut HashSet<String>,
) -> Result<()> {
    match schema {
        Value::Object(map) => {
            if let Some(header) = map.get("x-mcp-header") {
                ensure!(
                    property && reachable,
                    "x-mcp-header must be on a properties-only path"
                );
                let name = header.as_str().context("x-mcp-header must be a string")?;
                ensure!(
                    !name.is_empty() && HeaderName::from_bytes(name.as_bytes()).is_ok(),
                    "x-mcp-header must be a nonempty HTTP token"
                );
                ensure!(
                    seen.insert(name.to_ascii_lowercase()),
                    "duplicate x-mcp-header annotation"
                );
                ensure!(
                    matches!(
                        map.get("type").and_then(Value::as_str),
                        Some("string" | "integer" | "boolean")
                    ),
                    "x-mcp-header requires string, integer, or boolean type"
                );
            }
            for (key, child) in map {
                match key.as_str() {
                    "properties" | "$defs" | "definitions" | "dependentSchemas"
                    | "patternProperties" => {
                        if let Some(properties) = child.as_object() {
                            for child in properties.values() {
                                annotations(
                                    child,
                                    key == "properties",
                                    reachable && key == "properties",
                                    seen,
                                )?;
                            }
                        }
                    }
                    "default" | "const" | "enum" | "examples" | "x-mcp-header" => {}
                    _ => annotations(child, false, false, seen)?,
                }
            }
        }
        Value::Array(values) => {
            for child in values {
                annotations(child, false, false, seen)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn request_headers(
    method: &str,
    params: &Value,
    tool_schema: Option<&Value>,
) -> Result<HashMap<HeaderName, HeaderValue>> {
    let mut headers = HashMap::new();
    headers.insert(
        HeaderName::from_static("mcp-protocol-version"),
        HeaderValue::from_static(crate::mcp::PROTOCOL_VERSION),
    );
    headers.insert(
        HeaderName::from_static("mcp-method"),
        HeaderValue::from_str(method)?,
    );
    let name = match method {
        "tools/call" | "prompts/get" => params.get("name"),
        "resources/read" => params.get("uri"),
        "tasks/get" | "tasks/cancel" => params.get("taskId"),
        _ => None,
    };
    if let Some(name) = name.and_then(Value::as_str) {
        headers.insert(HeaderName::from_static("mcp-name"), encode_header(name)?);
    }
    if let Some(schema) = tool_schema {
        validate_annotations(schema)?;
        mirror_properties(
            schema,
            params.get("arguments").unwrap_or(&Value::Null),
            &mut headers,
        )?;
    }
    Ok(headers)
}

fn mirror_properties(
    schema: &Value,
    arguments: &Value,
    headers: &mut HashMap<HeaderName, HeaderValue>,
) -> Result<()> {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Ok(());
    };
    for (key, schema) in properties {
        let Some(value) = arguments.get(key) else {
            continue;
        };
        if let Some(name) = schema.get("x-mcp-header").and_then(Value::as_str) {
            let text = match value {
                Value::String(value) => value.clone(),
                Value::Bool(value) => value.to_string(),
                Value::Number(value) => {
                    let integer = value
                        .as_f64()
                        .context("header parameter must be a safe integer")?;
                    ensure!(
                        integer.fract() == 0.0
                            && (-9_007_199_254_740_991.0..=9_007_199_254_740_991.0)
                                .contains(&integer),
                        "header parameter must be a JavaScript-safe integer"
                    );
                    (integer as i64).to_string()
                }
                _ => bail!("header parameter must be a primitive value"),
            };
            let header = HeaderName::from_bytes(format!("mcp-param-{name}").as_bytes())?;
            headers.insert(header, encode_header(&text)?);
        }
        mirror_properties(schema, value, headers)?;
    }
    Ok(())
}

fn encode_header(value: &str) -> Result<HeaderValue> {
    if value.starts_with([' ', '\t'])
        || value.ends_with([' ', '\t'])
        || value.bytes().any(|byte| !(0x20..=0x7e).contains(&byte))
        || (value.starts_with("=?base64?") && value.ends_with("?="))
    {
        Ok(HeaderValue::from_str(&format!(
            "=?base64?{}?=",
            BASE64_STANDARD.encode(value)
        ))?)
    } else {
        Ok(HeaderValue::from_str(value)?)
    }
}

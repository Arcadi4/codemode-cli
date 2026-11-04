use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
};

use anyhow::{Context, Result, bail, ensure};
use base64::{
    alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
    read::DecoderReader,
};
use serde_json::{Value, json};

/// Select by the originating operation, never by ambiguous extension field names.
#[derive(Clone, Copy, Debug)]
pub enum OutputKind {
    Tool,
    Prompt,
    Resources,
}

pub fn is_tool_error(result: &Value) -> bool {
    result.get("isError").and_then(Value::as_bool) == Some(true)
}

pub fn render(
    result: Value,
    kind: OutputKind,
    json_output: bool,
    out: &mut impl Write,
) -> Result<()> {
    let result = normalize(result, kind)?;
    if json_output || result.get("resultType").and_then(Value::as_str) == Some("task") {
        write_json(&result, out)?;
    } else {
        match kind {
            OutputKind::Tool => {
                if let Some(structured) = result.get("structuredContent") {
                    let duplicate =
                        result
                            .get("content")
                            .and_then(Value::as_array)
                            .is_none_or(|blocks| {
                                blocks.iter().all(|block| {
                                    block.get("type").and_then(Value::as_str) == Some("text")
                                        && block.get("text").and_then(Value::as_str).is_some_and(
                                            |text| {
                                                serde_json::from_str::<Value>(text)
                                                    .is_ok_and(|value| &value == structured)
                                            },
                                        )
                                })
                            });
                    write_json(if duplicate { structured } else { &result }, out)?;
                } else if let Some(blocks) = result.get("content").and_then(Value::as_array) {
                    for block in blocks {
                        write_content(block, out)?;
                    }
                }
            }
            OutputKind::Prompt => {
                for (index, message) in result["messages"]
                    .as_array()
                    .context("prompt is missing messages")?
                    .iter()
                    .enumerate()
                {
                    if index != 0 {
                        writeln!(out)?;
                    }
                    writeln!(
                        out,
                        "{}:",
                        message["role"]
                            .as_str()
                            .context("prompt message is missing role")?
                    )?;
                    write_content(&message["content"], out)?;
                }
            }
            OutputKind::Resources => {
                for resource in result["contents"]
                    .as_array()
                    .context("resource result is missing contents")?
                {
                    write_resource(resource, out)?;
                }
            }
        }
    }
    out.flush()?;
    Ok(())
}

/// Replace base64 fields with {"path": ...} descriptors, preserving all other
/// fields, including server-defined `path` fields. Files survive successful use.
pub fn normalize(mut result: Value, kind: OutputKind) -> Result<Value> {
    ensure!(result.is_object(), "MCP result must be an object");
    if result.get("resultType").and_then(Value::as_str) == Some("task") {
        return Ok(result);
    }
    let mut artifacts = Artifacts::default();
    match kind {
        OutputKind::Tool => {
            if let Some(error) = result.get("isError") {
                ensure!(error.is_boolean(), "tool isError must be a boolean");
            }
            if let Some(content) = result.get_mut("content") {
                for block in content
                    .as_array_mut()
                    .context("tool content must be an array")?
                {
                    normalize_content(block, &mut artifacts)?;
                }
            } else {
                ensure!(
                    result.get("structuredContent").is_some(),
                    "tool result requires content or structuredContent"
                );
            }
        }
        OutputKind::Prompt => {
            for message in result
                .get_mut("messages")
                .and_then(Value::as_array_mut)
                .context("prompt result requires a messages array")?
            {
                ensure!(
                    matches!(
                        message.get("role").and_then(Value::as_str),
                        Some("user" | "assistant")
                    ),
                    "invalid prompt message role"
                );
                normalize_content(
                    message
                        .get_mut("content")
                        .context("prompt message requires content")?,
                    &mut artifacts,
                )?;
            }
        }
        OutputKind::Resources => {
            for resource in result
                .get_mut("contents")
                .and_then(Value::as_array_mut)
                .context("resource result requires a contents array")?
            {
                normalize_resource(resource, &mut artifacts)?;
            }
        }
    }
    artifacts.commit()?;
    Ok(result)
}

fn normalize_content(block: &mut Value, artifacts: &mut Artifacts) -> Result<()> {
    match block
        .get("type")
        .and_then(Value::as_str)
        .context("content block requires a string type")?
    {
        "text" => ensure!(
            block.get("text").is_some_and(Value::is_string),
            "text content requires a string text field"
        ),
        "image" | "audio" => {
            let mime = block
                .get("mimeType")
                .and_then(Value::as_str)
                .context("binary content requires mimeType")?;
            let data = block
                .get("data")
                .and_then(Value::as_str)
                .context("binary content requires base64 data")?;
            let path = artifacts.stage(data, Some(mime))?;
            block["data"] = json!({"path":path});
        }
        "resource" => normalize_resource(
            block
                .get_mut("resource")
                .context("embedded resource is missing resource")?,
            artifacts,
        )?,
        "resource_link" => {
            ensure!(
                block.get("uri").is_some_and(Value::is_string)
                    && block.get("name").is_some_and(Value::is_string),
                "resource link requires uri and name strings"
            );
        }
        other => bail!("unsupported MCP content type {other:?}"),
    }
    Ok(())
}

fn normalize_resource(resource: &mut Value, artifacts: &mut Artifacts) -> Result<()> {
    ensure!(
        resource.get("uri").is_some_and(Value::is_string),
        "resource contents require a URI"
    );
    let text = resource.get("text").is_some_and(Value::is_string);
    let blob = resource.get("blob").and_then(Value::as_str);
    // TextResourceContents | BlobResourceContents is anyOf: both are valid.
    ensure!(
        text || blob.is_some(),
        "resource contents require text or a base64 blob"
    );
    if let Some(blob) = blob {
        let mime = resource.get("mimeType").and_then(Value::as_str);
        let path = artifacts.stage(blob, mime)?;
        resource["blob"] = json!({"path":path});
    }
    Ok(())
}

fn write_content(block: &Value, out: &mut impl Write) -> Result<()> {
    match block["type"].as_str() {
        Some("text") => write_text(block["text"].as_str().context("missing text")?, out),
        Some("image" | "audio") => write_text(
            block["data"]["path"]
                .as_str()
                .context("missing artifact path")?,
            out,
        ),
        Some("resource") => write_resource(&block["resource"], out),
        Some("resource_link") => {
            write_text(block["uri"].as_str().context("missing resource URI")?, out)
        }
        _ => bail!("unsupported MCP content block"),
    }
}

fn write_resource(resource: &Value, out: &mut impl Write) -> Result<()> {
    if let Some(text) = resource.get("text").and_then(Value::as_str) {
        write_text(text, out)?;
    }
    if let Some(path) = resource
        .get("blob")
        .and_then(|blob| blob.get("path"))
        .and_then(Value::as_str)
    {
        write_text(path, out)?;
    }
    Ok(())
}

fn write_text(text: &str, out: &mut impl Write) -> Result<()> {
    out.write_all(text.as_bytes())?;
    if !text.ends_with('\n') {
        writeln!(out)?;
    }
    Ok(())
}

fn write_json(value: &Value, out: &mut impl Write) -> Result<()> {
    serde_json::to_writer_pretty(&mut *out, value).map_err(|error| {
        if let Some(kind) = error.io_error_kind() {
            anyhow::Error::from(io::Error::new(kind, error))
        } else {
            error.into()
        }
    })?;
    writeln!(out)?;
    Ok(())
}

#[derive(Default)]
struct Artifacts {
    files: Vec<tempfile::NamedTempFile>,
}

impl Artifacts {
    fn stage(&mut self, data: &str, mime: Option<&str>) -> Result<String> {
        let extension = mime
            .and_then(|mime| {
                mime_guess::get_mime_extensions_str(mime.split(';').next().unwrap_or("").trim())
            })
            .and_then(|extensions| {
                extensions
                    .iter()
                    .copied()
                    .find(|extension| extension.bytes().all(|byte| byte.is_ascii_alphanumeric()))
            })
            .unwrap_or("bin");
        let mut file = tempfile::Builder::new()
            .prefix("cm-")
            .suffix(&format!(".{extension}"))
            .tempfile()?;
        const ENGINE: GeneralPurpose = GeneralPurpose::new(
            &alphabet::STANDARD,
            GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
        );
        // Stream decoding keeps large binary results from requiring another full-size allocation.
        io::copy(&mut DecoderReader::new(data.as_bytes(), &ENGINE), &mut file)
            .context("invalid base64 content or artifact write failure")?;
        let path: PathBuf = file
            .path()
            .canonicalize()
            .context("cannot resolve artifact path")?;
        let path = path
            .to_str()
            .context("temporary directory is not UTF-8; set TMPDIR to a UTF-8 path")?
            .to_owned();
        self.files.push(file);
        Ok(path)
    }

    fn commit(self) -> Result<()> {
        let mut retained = Vec::with_capacity(self.files.len());
        for file in self.files {
            match file.keep() {
                Ok((_file, path)) => retained.push(path),
                Err(error) => {
                    for path in retained {
                        let _ = fs::remove_file(path);
                    }
                    return Err(error.error).context("cannot retain temporary artifact");
                }
            }
        }
        Ok(())
    }
}

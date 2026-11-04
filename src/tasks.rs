use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use crate::{
    host::Host,
    output::{self, OutputKind},
};

pub const EXTENSION: &str = "io.modelcontextprotocol/tasks";

pub fn validate_receipt(result: &Value) -> Result<()> {
    ensure!(
        result.get("resultType").and_then(Value::as_str) == Some("task"),
        "expected a Tasks extension receipt"
    );
    validate_metadata(result)?;
    ensure!(
        result["status"] != "input_required",
        "task {:?} requires unsupported client input; cm never fulfills elicitation, sampling, or roots",
        result["taskId"]
    );
    // Result objects may contain extension fields. Preserve them, rather than
    // rejecting fields that happen to share names with a synchronous result.
    Ok(())
}

pub async fn get(host: &mut Host, server: &str, task_id: &str) -> Result<Value> {
    host.require_extension(server, EXTENSION).await?;
    let mut result = host
        .request(server, "tasks/get", json!({"taskId":task_id}))
        .await?;
    ensure!(
        result["resultType"] == "complete",
        "tasks/get requires a complete result"
    );
    validate_metadata(&result)?;
    ensure!(
        result["taskId"].as_str() == Some(task_id),
        "server returned a different task ID"
    );
    match result["status"].as_str() {
        Some("input_required") => bail!(
            "task {task_id:?} on {server:?} requires unsupported client input; cm does not support elicitation, sampling, or roots; use `cm tasks cancel` to request cancellation"
        ),
        Some("completed") => {
            let nested = result
                .get_mut("result")
                .context("completed task is missing its result")?;
            ensure!(
                nested.is_object(),
                "completed task result must be an object"
            );
            *nested = output::normalize(std::mem::take(nested), OutputKind::Tool)?;
        }
        Some("failed") => {
            let error = result
                .get("error")
                .context("failed task is missing its error")?;
            ensure!(
                error.get("code").and_then(Value::as_i64).is_some()
                    && error.get("message").is_some_and(Value::is_string),
                "failed task requires a JSON-RPC error"
            );
        }
        _ => {}
    }
    Ok(result)
}

pub async fn cancel(host: &mut Host, server: &str, task_id: &str) -> Result<Value> {
    host.require_extension(server, EXTENSION).await?;
    let result = host
        .request(server, "tasks/cancel", json!({"taskId":task_id}))
        .await?;
    ensure!(
        result["resultType"] == "complete",
        "tasks/cancel requires a complete acknowledgement"
    );
    Ok(result)
}

pub fn failed(result: &Value) -> bool {
    matches!(
        result.get("status").and_then(Value::as_str),
        Some("failed" | "cancelled")
    ) || (result["status"] == "completed"
        && result.get("result").is_some_and(output::is_tool_error))
}

fn validate_metadata(result: &Value) -> Result<()> {
    for field in ["taskId", "createdAt", "lastUpdatedAt"] {
        ensure!(
            result.get(field).is_some_and(Value::is_string),
            "task metadata requires string {field}"
        );
    }
    ensure!(
        matches!(
            result.get("status").and_then(Value::as_str),
            Some("working" | "input_required" | "completed" | "failed" | "cancelled")
        ),
        "unsupported task status"
    );
    let ttl = result
        .get("ttlMs")
        .context("task metadata requires ttlMs")?;
    ensure!(
        ttl.is_null() || nonnegative_integer(ttl),
        "task ttlMs must be null or a nonnegative integer"
    );
    if let Some(interval) = result.get("pollIntervalMs") {
        ensure!(
            nonnegative_integer(interval),
            "task pollIntervalMs must be a nonnegative integer"
        );
    }
    Ok(())
}

fn nonnegative_integer(value: &Value) -> bool {
    value
        .as_f64()
        .is_some_and(|number| number.is_finite() && number >= 0.0 && number.fract() == 0.0)
}

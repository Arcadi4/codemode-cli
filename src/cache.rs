use std::{
    fs,
    io::Write,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const CACHEABLE: &[&str] = &[
    "server/discover",
    "tools/list",
    "prompts/list",
    "resources/list",
    "resources/templates/list",
    "resources/read",
];

pub struct Cache {
    directory: PathBuf,
    partition: String,
    refresh: bool,
}

#[derive(Serialize, Deserialize)]
struct Entry<T = Value> {
    received_ms: u64,
    ttl_ms: u64,
    result: T,
}

impl Cache {
    pub fn new(authorization_context: &[u8], refresh: bool) -> Result<Self> {
        let directory = if let Some(path) = std::env::var_os("CM_CACHE_DIR") {
            PathBuf::from(path)
        } else if let Some(path) = std::env::var_os("XDG_CACHE_HOME") {
            PathBuf::from(path).join("codemode")
        } else {
            directories::BaseDirs::new()
                .context("cannot determine the home directory")?
                .home_dir()
                .join(".cache/codemode")
        };
        let partition = hex::encode(Sha256::digest(authorization_context));
        Ok(Self {
            directory,
            partition,
            refresh,
        })
    }

    fn path(&self, method: &str, params: &Value) -> Result<PathBuf> {
        let mut hash = Sha256::new();
        hash.update(b"cm-cache-v1\0");
        hash.update(self.partition.as_bytes());
        hash.update(b"\0");
        hash.update(method.as_bytes());
        hash.update(b"\0");
        serde_json::to_writer(&mut HashWriter(&mut hash), params)?;
        Ok(self
            .directory
            .join(format!("{}.json", hex::encode(hash.finalize()))))
    }

    pub fn get(&self, method: &str, params: &Value) -> Result<Option<Value>> {
        if self.refresh || !cacheable(method, params) {
            return Ok(None);
        }
        let path = self.path(method, params)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("cannot read response cache"),
        };
        // A corrupt/interrupted cache is a miss, never a substitute for a server response.
        let Ok(entry) = serde_json::from_slice::<Entry>(&bytes) else {
            return Ok(None);
        };
        let now = now_ms();
        if now < entry.received_ms || now - entry.received_ms >= entry.ttl_ms {
            return Ok(None);
        }
        Ok(Some(entry.result))
    }

    pub fn put(&self, method: &str, params: &Value, result: &Value) -> Result<()> {
        let received_ms = now_ms();
        if !cacheable(method, params)
            || result
                .get("resultType")
                .and_then(Value::as_str)
                .unwrap_or("complete")
                != "complete"
            || result.get("isError").and_then(Value::as_bool) == Some(true)
        {
            return Ok(());
        }
        let ttl_ms = result
            .get("ttlMs")
            .and_then(|value| {
                value.as_u64().or_else(|| {
                    value
                        .as_f64()
                        .filter(|value| {
                            value.is_finite()
                                && *value >= 0.0
                                && value.fract() == 0.0
                                && *value < u64::MAX as f64
                        })
                        .map(|value| value as u64)
                })
            })
            .unwrap_or(0);
        if ttl_ms == 0
            || !matches!(
                result.get("cacheScope").and_then(Value::as_str),
                Some("public" | "private")
            )
        {
            return Ok(());
        }
        fs::create_dir_all(&self.directory).context("cannot create response cache directory")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.directory, fs::Permissions::from_mode(0o700))?;
        }
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
        serde_json::to_writer(
            &mut file,
            &Entry {
                received_ms,
                ttl_ms,
                result,
            },
        )?;
        file.flush()?;
        file.persist(self.path(method, params)?)
            .context("cannot save response cache")?;
        Ok(())
    }

    pub fn invalidate(&self, method: &str, params: &Value) -> Result<()> {
        match fs::remove_file(self.path(method, params)?) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

pub fn cacheable(method: &str, params: &Value) -> bool {
    CACHEABLE.contains(&method)
        && params.get("inputResponses").is_none()
        && params.get("requestState").is_none()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

struct HashWriter<'a>(&'a mut Sha256);
impl Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

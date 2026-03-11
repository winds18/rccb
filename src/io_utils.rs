use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::Client;
use serde::Serialize;
use serde_json::Value;
use sysinfo::{Pid, System};

use crate::constants::SUPPORTED_PROVIDERS;
use crate::layout::run_dir;
use crate::types::InstanceState;

static REQ_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn resolve_project_dir(input: &Path) -> Result<PathBuf> {
    let path = if input.is_absolute() {
        input.to_path_buf()
    } else {
        std::env::current_dir()?.join(input)
    };

    if !path.exists() {
        bail!("project dir does not exist: {}", path.display());
    }
    if !path.is_dir() {
        bail!("project dir is not a directory: {}", path.display());
    }

    Ok(path.canonicalize().unwrap_or(path))
}

pub fn write_state(path: &Path, state: &InstanceState) -> Result<()> {
    write_json_pretty(path, state)
}

pub fn load_state(path: &Path) -> Result<InstanceState> {
    let mut raw = String::new();
    File::open(path)
        .with_context(|| format!("open state failed: {}", path.display()))?
        .read_to_string(&mut raw)
        .with_context(|| format!("read state failed: {}", path.display()))?;

    let s: InstanceState = serde_json::from_str(&raw)
        .with_context(|| format!("parse state failed: {}", path.display()))?;
    Ok(s)
}

pub fn load_all_states(project_dir: &Path) -> Result<Vec<InstanceState>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(run_dir(project_dir))? {
        let e = entry?;
        let path = e.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        match load_state(&path) {
            Ok(s) => out.push(s),
            Err(err) => {
                eprintln!("warn: skip invalid state {}: {}", path.display(), err);
            }
        }
    }
    Ok(out)
}

pub fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("invalid path: {}", path.display()))?;
    fs::create_dir_all(parent)?;

    let tmp = path.with_extension("tmp");
    let data = serde_json::to_vec_pretty(value)?;
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&data)?;
        f.flush()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn write_line(path: PathBuf, line: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open log failed: {}", path.display()))?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

pub fn read_stdin_all() -> Result<String> {
    let mut buf = String::new();
    io::stdin()
        .read_to_string(&mut buf)
        .context("read stdin failed")?;
    Ok(buf)
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs()
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as u64
}

pub fn is_process_alive(pid: u32) -> bool {
    let mut sys = System::new_all();
    sys.refresh_processes();
    sys.process(Pid::from_u32(pid)).is_some()
}

pub fn build_http_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("build http client failed")
}

pub fn normalize_provider_list(raw: &[String]) -> Result<Vec<String>> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for item in raw {
        let p = normalize_provider(item)?;
        if !out.iter().any(|x| x == &p) {
            out.push(p);
        }
    }

    Ok(out)
}

pub fn normalize_provider(input: &str) -> Result<String> {
    let p = input.trim().to_lowercase();
    if p.is_empty() {
        bail!("provider cannot be empty");
    }
    if SUPPORTED_PROVIDERS.iter().any(|x| *x == p) {
        return Ok(p);
    }

    bail!(
        "unsupported provider `{}`. supported: {}",
        input,
        SUPPORTED_PROVIDERS.join(", ")
    )
}

pub fn parse_listen_addr(input: &str) -> Result<(String, u16)> {
    let value = input.trim();
    if value.is_empty() {
        return Ok(("127.0.0.1".to_string(), 0));
    }

    if let Some((host, port_str)) = value.rsplit_once(':') {
        let host = if host.is_empty() { "127.0.0.1" } else { host };
        let port: u16 = port_str
            .parse()
            .with_context(|| format!("invalid listen port: {}", port_str))?;
        return Ok((host.to_string(), port));
    }

    Ok((value.to_string(), 0))
}

pub fn normalize_connect_host(host: &str) -> String {
    let h = host.trim();
    if h == "0.0.0.0" || h.is_empty() {
        return "127.0.0.1".to_string();
    }
    if h == "::" || h == "[::]" {
        return "::1".to_string();
    }
    h.to_string()
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut f) = File::open("/dev/urandom") {
        if f.read_exact(&mut bytes).is_ok() {
            return bytes.iter().map(|b| format!("{:02x}", b)).collect();
        }
    }

    let fallback = format!(
        "{}-{}-{}",
        now_unix_ms(),
        std::process::id(),
        REQ_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    hex_encode(fallback.as_bytes())
}

pub fn make_req_id() -> String {
    let counter = REQ_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    let millis = now_unix_ms();
    let secs = millis / 1000;
    let ms = millis % 1000;
    format!("{}-{:03}-{}-{}", secs, ms, std::process::id(), counter)
}

fn hex_encode(input: &[u8]) -> String {
    input.iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn update_task_status(
    task_file: &Path,
    status: &str,
    started_at: Option<u64>,
    completed_at: Option<u64>,
    exit_code: Option<i32>,
    reply: Option<&str>,
) -> Result<()> {
    let mut raw = String::new();
    File::open(task_file)
        .with_context(|| format!("open task file failed: {}", task_file.display()))?
        .read_to_string(&mut raw)
        .with_context(|| format!("read task file failed: {}", task_file.display()))?;

    let mut val: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse task file failed: {}", task_file.display()))?;

    val["status"] = serde_json::json!(status);
    if let Some(v) = started_at {
        val["started_at_unix"] = serde_json::json!(v);
    }
    if let Some(v) = completed_at {
        val["completed_at_unix"] = serde_json::json!(v);
    }
    if let Some(v) = exit_code {
        val["exit_code"] = serde_json::json!(v);
    }
    if let Some(v) = reply {
        val["reply"] = serde_json::json!(v);
    }

    write_json_pretty(task_file, &val)
}

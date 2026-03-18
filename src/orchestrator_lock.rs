use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::io_utils::{now_unix, write_json_pretty};
use crate::layout::{ensure_project_layout, run_dir, sanitize_filename, sanitize_instance};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorInflightLock {
    pub instance: String,
    pub orchestrator: String,
    pub executor: String,
    pub req_id: String,
    pub status: String,
    pub created_at_unix: u64,
    pub updated_at_unix: u64,
}

pub fn inflight_dir(project_dir: &Path, instance: &str, orchestrator: &str) -> PathBuf {
    run_dir(project_dir)
        .join(sanitize_instance(instance))
        .join("orchestrator")
        .join(sanitize_filename(orchestrator))
        .join("inflight")
}

pub fn inflight_path(
    project_dir: &Path,
    instance: &str,
    orchestrator: &str,
    req_id: &str,
) -> PathBuf {
    inflight_dir(project_dir, instance, orchestrator)
        .join(format!("{}.json", sanitize_filename(req_id)))
}

pub fn mark_inflight(
    project_dir: &Path,
    instance: &str,
    orchestrator: &str,
    executor: &str,
    req_id: &str,
    status: &str,
) -> Result<()> {
    ensure_project_layout(project_dir)?;
    let path = inflight_path(project_dir, instance, orchestrator, req_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let now = now_unix();
    let created_at_unix = fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<OrchestratorInflightLock>(&raw).ok())
        .map(|v| v.created_at_unix)
        .unwrap_or(now);
    let record = OrchestratorInflightLock {
        instance: instance.trim().to_string(),
        orchestrator: orchestrator.trim().to_string(),
        executor: executor.trim().to_string(),
        req_id: req_id.trim().to_string(),
        status: status.trim().to_string(),
        created_at_unix,
        updated_at_unix: now,
    };
    write_json_pretty(&path, &record)
}

pub fn clear_inflight(
    project_dir: &Path,
    instance: &str,
    orchestrator: &str,
    req_id: &str,
) -> Result<()> {
    let path = inflight_path(project_dir, instance, orchestrator, req_id);
    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("删除编排等待锁失败：{}", path.display()))?;
    }
    Ok(())
}

pub fn load_inflight(
    project_dir: &Path,
    instance: &str,
    orchestrator: &str,
) -> Result<Vec<OrchestratorInflightLock>> {
    let dir = inflight_dir(project_dir, instance, orchestrator);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for entry in
        fs::read_dir(&dir).with_context(|| format!("读取编排等待锁目录失败：{}", dir.display()))?
    {
        let entry = match entry {
            Ok(v) => v,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let raw = match fs::read_to_string(&path) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let parsed = match serde_json::from_str::<OrchestratorInflightLock>(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        out.push(parsed);
    }
    out.sort_by(|a, b| a.created_at_unix.cmp(&b.created_at_unix));
    Ok(out)
}

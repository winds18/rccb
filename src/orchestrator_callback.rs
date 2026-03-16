use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::io_utils::{read_stdin_all, write_line};
use crate::layout::{launcher_meta_path, logs_instance_dir};
use crate::provider::{
    dispatch_text_to_pane, PaneBackend as ProviderPaneBackend, PaneDispatchTarget,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrchestratorNoticeKind {
    Started,
    Progress,
    Result,
}

impl OrchestratorNoticeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Progress => "progress",
            Self::Result => "result",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "started" => Ok(Self::Started),
            "progress" => Ok(Self::Progress),
            "result" => Ok(Self::Result),
            other => Err(anyhow!("不支持的编排者通知类型：{}", other)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OrchestratorNoticeRequest {
    pub project_dir: PathBuf,
    pub instance: String,
    pub orchestrator: String,
    pub req_id: String,
    pub kind: OrchestratorNoticeKind,
    pub body: String,
}

#[derive(Debug, Clone, Deserialize)]
struct LauncherMetaView {
    backend: String,
    #[serde(default)]
    backend_bin: Option<String>,
    #[serde(default)]
    providers: Vec<LauncherProviderMetaView>,
}

#[derive(Debug, Clone, Deserialize)]
struct LauncherProviderMetaView {
    provider: String,
    #[serde(default)]
    pane_id: Option<String>,
    #[serde(default)]
    pane_title: Option<String>,
}

pub fn cmd_orchestrator_notify(
    project_dir: &Path,
    instance: &str,
    orchestrator: &str,
    req_id: &str,
    kind: &str,
) -> Result<()> {
    let body = read_stdin_all()?;
    let kind = OrchestratorNoticeKind::parse(kind)?;
    deliver_orchestrator_notice(&OrchestratorNoticeRequest {
        project_dir: project_dir.to_path_buf(),
        instance: instance.trim().to_string(),
        orchestrator: orchestrator.trim().to_string(),
        req_id: req_id.trim().to_string(),
        kind,
        body,
    })
}

pub fn deliver_orchestrator_notice(req: &OrchestratorNoticeRequest) -> Result<()> {
    let body = req.body.trim();
    if body.is_empty() {
        return Ok(());
    }

    let attempts = orchestrator_callback_retries();
    let delay_ms = orchestrator_callback_retry_delay_ms();
    let mut last_err = None;
    for attempt in 0..attempts {
        match resolve_orchestrator_dispatch_target(
            &req.project_dir,
            &req.instance,
            &req.orchestrator,
        ) {
            Ok(Some(target)) => match dispatch_text_to_pane(&target, body) {
                Ok(()) => return Ok(()),
                Err(err) => {
                    last_err = Some(err.to_string());
                    log_notice(
                        &req.project_dir,
                        &req.instance,
                        &format!(
                            "[WARN] orchestrator callback send failed orchestrator={} req_id={} kind={} attempt={}/{} err={}",
                            req.orchestrator,
                            req.req_id,
                            req.kind.as_str(),
                            attempt + 1,
                            attempts,
                            err
                        ),
                    );
                }
            },
            Ok(None) => {
                last_err = Some("未找到编排者 pane".to_string());
                log_notice(
                    &req.project_dir,
                    &req.instance,
                    &format!(
                        "[WARN] orchestrator callback target missing orchestrator={} req_id={} kind={} attempt={}/{}",
                        req.orchestrator,
                        req.req_id,
                        req.kind.as_str(),
                        attempt + 1,
                        attempts
                    ),
                );
            }
            Err(err) => {
                last_err = Some(err.to_string());
                log_notice(
                    &req.project_dir,
                    &req.instance,
                    &format!(
                        "[WARN] orchestrator callback resolve failed orchestrator={} req_id={} kind={} attempt={}/{} err={}",
                        req.orchestrator,
                        req.req_id,
                        req.kind.as_str(),
                        attempt + 1,
                        attempts,
                        err
                    ),
                );
            }
        }

        if attempt + 1 < attempts && delay_ms > 0 {
            thread::sleep(Duration::from_millis(delay_ms));
        }
    }

    Err(anyhow!(
        "编排者通知投递失败：orchestrator={} req_id={} kind={} err={}",
        req.orchestrator,
        req.req_id,
        req.kind.as_str(),
        last_err.unwrap_or_else(|| "unknown".to_string())
    ))
}

fn resolve_orchestrator_dispatch_target(
    project_dir: &Path,
    instance: &str,
    orchestrator: &str,
) -> Result<Option<PaneDispatchTarget>> {
    let path = launcher_meta_path(project_dir, instance);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("read launcher meta failed: {}", path.display()))?;
    let meta: LauncherMetaView = serde_json::from_str(&raw)
        .with_context(|| format!("parse launcher meta failed: {}", path.display()))?;
    let entry = meta
        .providers
        .iter()
        .find(|p| p.provider.trim().eq_ignore_ascii_case(orchestrator));
    let Some(entry) = entry else {
        return Ok(None);
    };

    let pane_id = entry
        .pane_id
        .clone()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let pane_title = entry
        .pane_title
        .clone()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());

    match meta.backend.trim().to_ascii_lowercase().as_str() {
        "tmux" => {
            let pane_id = pane_id
                .filter(|pane| tmux_pane_alive(pane))
                .or_else(|| pane_title.as_deref().and_then(find_tmux_pane_by_title));
            let Some(pane_id) = pane_id else {
                return Ok(None);
            };
            Ok(Some(PaneDispatchTarget {
                backend: ProviderPaneBackend::Tmux,
                pane_id,
            }))
        }
        "wezterm" => {
            let Some(pane_id) = pane_id else {
                return Ok(None);
            };
            let bin = meta
                .backend_bin
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| {
                    env::var("RCCB_WEZTERM_BIN").unwrap_or_else(|_| "wezterm".to_string())
                });
            Ok(Some(PaneDispatchTarget {
                backend: ProviderPaneBackend::Wezterm { bin },
                pane_id,
            }))
        }
        other => Err(anyhow!("不支持的 launcher backend：{}", other)),
    }
}

fn tmux_pane_alive(pane_id: &str) -> bool {
    Command::new("tmux")
        .args(["display-message", "-p", "-t", pane_id.trim(), "#{pane_id}"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn find_tmux_pane_by_title(title: &str) -> Option<String> {
    let output = Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{pane_id}\t#{pane_title}"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let mut parts = line.splitn(2, '\t');
        let pane_id = parts.next()?.trim();
        let pane_title = parts.next().unwrap_or_default().trim();
        if pane_id.is_empty() {
            continue;
        }
        if pane_title == title.trim() {
            return Some(pane_id.to_string());
        }
    }
    None
}

fn orchestrator_callback_retries() -> usize {
    match env::var("RCCB_ORCHESTRATOR_CALLBACK_RETRIES") {
        Ok(raw) => raw.trim().parse::<usize>().unwrap_or(6).clamp(1, 40),
        Err(_) => 6,
    }
}

fn orchestrator_callback_retry_delay_ms() -> u64 {
    match env::var("RCCB_ORCHESTRATOR_CALLBACK_RETRY_DELAY_MS") {
        Ok(raw) => raw.trim().parse::<u64>().unwrap_or(350).min(5000),
        Err(_) => 350,
    }
}

fn log_notice(project_dir: &Path, instance: &str, line: &str) {
    let path = logs_instance_dir(project_dir, instance).join("daemon.log");
    let _ = write_line(path, line);
}

#[cfg(test)]
mod tests {
    use super::OrchestratorNoticeKind;

    #[test]
    fn orchestrator_notice_kind_parse_accepts_known_values() {
        assert_eq!(
            OrchestratorNoticeKind::parse("started").expect("started"),
            OrchestratorNoticeKind::Started
        );
        assert_eq!(
            OrchestratorNoticeKind::parse("progress").expect("progress"),
            OrchestratorNoticeKind::Progress
        );
        assert_eq!(
            OrchestratorNoticeKind::parse("result").expect("result"),
            OrchestratorNoticeKind::Result
        );
    }
}

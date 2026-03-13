use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::types::AskRequest;

const REQ_ID_PREFIX: &str = "RCCB_REQ_ID:";
const BEGIN_PREFIX: &str = "RCCB_BEGIN:";
const DONE_PREFIX: &str = "RCCB_DONE:";

#[derive(Debug, Clone)]
pub struct ProviderExecResult {
    pub exit_code: i32,
    pub reply: String,
    pub done_seen: bool,
    pub done_ms: Option<u64>,
    pub anchor_seen: bool,
    pub anchor_ms: Option<u64>,
    pub fallback_scan: bool,
    pub status: String,
    pub stderr: String,
    pub effective_timeout_s: f64,
    pub effective_quiet: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecMode {
    Bridge,
    Native,
    Stub,
}

#[derive(Debug, Clone)]
pub enum PaneBackend {
    Tmux,
    Wezterm { bin: String },
}

#[derive(Debug, Clone)]
pub struct PaneDispatchTarget {
    pub backend: PaneBackend,
    pub pane_id: String,
}

const RCCB_AUTOSTART_ENV_KEYS: &[&str] = &[
    "RCCB_ASKD_AUTOSTART",
    "RCCB_CASKD_AUTOSTART",
    "RCCB_GASKD_AUTOSTART",
    "RCCB_OASKD_AUTOSTART",
    "RCCB_LASKD_AUTOSTART",
    "RCCB_DASKD_AUTOSTART",
    "RCCB_CASKD",
    "RCCB_GASKD",
    "RCCB_OASKD",
    "RCCB_LASKD",
    "RCCB_DASKD",
    "RCCB_AUTO_CASKD",
    "RCCB_AUTO_GASKD",
    "RCCB_AUTO_OASKD",
    "RCCB_AUTO_LASKD",
    "RCCB_AUTO_DASKD",
];

enum PipeMsg {
    Stdout(String),
    Stderr(String),
    StdoutEof,
    StderrEof,
}

struct ProcessOutcome {
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: bool,
    canceled: bool,
    elapsed_ms: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct NativeProviderProfile {
    #[serde(default)]
    cmd: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    no_wrap: Option<bool>,
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    timeout_s: Option<f64>,
    #[serde(default)]
    quiet: Option<bool>,
}

pub fn execute_provider_request(
    req: &AskRequest,
    req_id: &str,
    mut on_delta: impl FnMut(String),
    should_cancel: impl Fn() -> bool,
    pane_target: Option<&PaneDispatchTarget>,
) -> Result<ProviderExecResult> {
    let mode = execution_mode();
    match mode {
        ExecMode::Stub => Ok(run_stub(req, req_id)),
        ExecMode::Bridge => {
            let wrapper = resolve_wrapper_path(&req.provider).with_context(|| {
                format!(
                    "provider `{}` 的 bridge wrapper 不存在，请设置 RCCB_{}_CMD 或 RCCB_BRIDGE_BIN_DIR",
                    req.provider,
                    req.provider.to_ascii_uppercase()
                )
            })?;
            let mut cmd = Command::new(&wrapper);
            cmd.current_dir(&req.work_dir)
                .arg("--sync")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .env("RCCB_CALLER", &req.caller)
                .env("RCCB_REQ_ID", req_id);
            apply_rccb_autostart_env(&mut cmd);
            if req.quiet {
                cmd.arg("--quiet");
            }
            if req.timeout_s >= 0.0 {
                cmd.arg("--timeout").arg(format!("{:.3}", req.timeout_s));
                cmd.env("RCCB_SYNC_TIMEOUT", format!("{:.3}", req.timeout_s));
            }

            let input = format!("{}\n", req.message);
            let timeout = timeout_for_request(req.timeout_s);
            let outcome =
                run_process_with_stream(cmd, &input, timeout, &mut on_delta, &should_cancel)
                    .with_context(|| {
                        format!(
                            "启动 bridge wrapper 失败：provider={} wrapper={}",
                            req.provider,
                            wrapper.display()
                        )
                    })?;
            Ok(build_exec_result(
                mode,
                req_id,
                outcome,
                req.timeout_s,
                req.quiet,
            ))
        }
        ExecMode::Native => {
            let work_dir = Path::new(&req.work_dir);
            let profile = load_native_profile(&req.provider, work_dir)
                .with_context(|| format!("加载原生 provider 配置失败：`{}`", req.provider))?;
            let effective_timeout_s =
                effective_native_timeout_s(&req.provider, req.timeout_s, profile.as_ref());
            let effective_quiet =
                effective_native_quiet(&req.provider, req.quiet, profile.as_ref());

            let binary = resolve_native_provider_cmd(&req.provider, work_dir, profile.as_ref())
                .with_context(|| {
                    format!(
                        "provider `{}` 的原生命令不存在，请设置 RCCB_{}_NATIVE_CMD",
                        req.provider,
                        req.provider.to_ascii_uppercase()
                    )
                })?;
            let prompt = if should_wrap_native_prompt(&req.provider, profile.as_ref()) {
                wrap_prompt_for_provider(&req.provider, &req.message, req_id)
            } else {
                req.message.trim_end().to_string()
            };

            if let Some(target) = pane_target {
                if native_should_use_pane_exec(&req.provider) {
                    let pane_prompt = wrap_prompt_for_provider(&req.provider, &req.message, req_id);
                    return execute_native_via_pane(
                        req_id,
                        &pane_prompt,
                        effective_timeout_s,
                        effective_quiet,
                        target,
                        &mut on_delta,
                        &should_cancel,
                    );
                }
            }

            let mut cmd = Command::new(&binary);
            cmd.current_dir(&req.work_dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .env("RCCB_CALLER", &req.caller)
                .env("RCCB_REQ_ID", req_id)
                .env("RCCB_NATIVE_PROVIDER", &req.provider);

            let (native_args, used_default_args) = native_args_for_provider(
                &req.provider,
                req,
                req_id,
                effective_timeout_s,
                profile.as_ref(),
            );
            for arg in native_args {
                cmd.arg(arg);
            }
            for (k, v) in native_env_for_provider(
                &req.provider,
                req,
                req_id,
                effective_timeout_s,
                profile.as_ref(),
            ) {
                cmd.env(k, v);
            }

            let input = if native_should_use_stdin(&req.provider, used_default_args) {
                format!("{}\n", prompt)
            } else {
                String::new()
            };
            let timeout = timeout_for_request(effective_timeout_s);
            let mut maybe_emit_delta = |chunk: String| {
                if !effective_quiet {
                    on_delta(chunk);
                }
            };
            let outcome = run_process_with_stream(
                cmd,
                &input,
                timeout,
                &mut maybe_emit_delta,
                &should_cancel,
            )
            .with_context(|| {
                format!(
                    "启动原生 provider 失败：provider={} cmd={}",
                    req.provider,
                    binary.display()
                )
            })?;

            Ok(build_exec_result(
                mode,
                req_id,
                outcome,
                effective_timeout_s,
                effective_quiet,
            ))
        }
    }
}

fn execution_mode() -> ExecMode {
    let raw = env::var("RCCB_EXEC_MODE").unwrap_or_else(|_| "native".to_string());
    match raw.trim().to_ascii_lowercase().as_str() {
        "stub" => ExecMode::Stub,
        "bridge" => ExecMode::Bridge,
        _ => ExecMode::Native,
    }
}

fn apply_rccb_autostart_env(cmd: &mut Command) {
    for key in RCCB_AUTOSTART_ENV_KEYS {
        cmd.env(key, "1");
    }
}

fn timeout_for_request(timeout_s: f64) -> Option<Duration> {
    if timeout_s < 0.0 {
        None
    } else {
        Some(Duration::from_secs_f64(timeout_s.max(0.1) + 5.0))
    }
}

fn native_should_use_pane_exec(provider: &str) -> bool {
    let key = format!("RCCB_{}_PANE_EXEC", provider.trim().to_ascii_uppercase());
    if let Some(v) = parse_env_bool(&key) {
        return v;
    }
    if let Some(v) = parse_env_bool("RCCB_PANE_EXEC") {
        return v;
    }
    matches!(
        provider.trim().to_ascii_lowercase().as_str(),
        "codex" | "opencode" | "gemini" | "droid"
    )
}

fn execute_native_via_pane(
    req_id: &str,
    pane_prompt: &str,
    effective_timeout_s: f64,
    effective_quiet: bool,
    target: &PaneDispatchTarget,
    on_delta: &mut dyn FnMut(String),
    should_cancel: &dyn Fn() -> bool,
) -> Result<ProviderExecResult> {
    let started = Instant::now();
    let poll_ms = parse_env_f64("RCCB_PANE_POLL_MS")
        .unwrap_or(300.0)
        .clamp(80.0, 3000.0) as u64;
    let capture_lines = parse_env_f64("RCCB_PANE_CAPTURE_LINES")
        .unwrap_or(800.0)
        .clamp(200.0, 4000.0) as i32;

    let previous_snapshot = capture_pane_text(target, capture_lines)
        .with_context(|| format!("下发前抓取 pane 内容失败：pane={}", target.pane_id))?;
    dispatch_text_to_pane(target, pane_prompt)
        .with_context(|| format!("向 pane 下发任务失败：pane={}", target.pane_id))?;

    let timeout = if effective_timeout_s < 0.0 {
        None
    } else {
        Some(Duration::from_secs_f64(effective_timeout_s.max(0.1)))
    };

    let mut transcript = String::new();
    let mut previous_window = pane_window_for_req(&previous_snapshot, req_id).unwrap_or_default();
    loop {
        if should_cancel() {
            return Ok(ProviderExecResult {
                exit_code: 130,
                reply: "请求已取消".to_string(),
                done_seen: false,
                done_ms: None,
                anchor_seen: true,
                anchor_ms: Some(0),
                fallback_scan: false,
                status: "canceled".to_string(),
                stderr: String::new(),
                effective_timeout_s,
                effective_quiet,
            });
        }

        if let Some(limit) = timeout {
            if started.elapsed() >= limit {
                return Ok(ProviderExecResult {
                    exit_code: 2,
                    reply: "请求超时".to_string(),
                    done_seen: false,
                    done_ms: None,
                    anchor_seen: true,
                    anchor_ms: Some(0),
                    fallback_scan: false,
                    status: "timeout".to_string(),
                    stderr: String::new(),
                    effective_timeout_s,
                    effective_quiet,
                });
            }
        }

        thread::sleep(Duration::from_millis(poll_ms));

        let current = match capture_pane_text(target, capture_lines) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let current_window = pane_window_for_req(&current, req_id).unwrap_or_default();
        let delta = pane_output_delta(&previous_window, &current_window);
        previous_window = current_window.clone();

        if !delta.is_empty() {
            transcript.push_str(&delta);
            if !transcript.ends_with('\n') {
                transcript.push('\n');
            }
            if !effective_quiet {
                on_delta(delta);
            }
        }

        if contains_done_line_for_req(&transcript, req_id)
            || contains_done_line_for_req(&current_window, req_id)
        {
            break;
        }
    }

    let mut reply = extract_reply_for_req(&transcript, req_id);
    if reply.trim().is_empty() {
        reply = extract_reply_for_req(&previous_window, req_id);
    }
    if reply.trim().is_empty() {
        reply = strip_done_text(&previous_window, req_id);
    }
    if reply.trim().is_empty() {
        reply = strip_done_text(&transcript, req_id);
    }

    Ok(ProviderExecResult {
        exit_code: 0,
        reply,
        done_seen: true,
        done_ms: Some(started.elapsed().as_millis() as u64),
        anchor_seen: true,
        anchor_ms: Some(0),
        fallback_scan: false,
        status: "completed".to_string(),
        stderr: String::new(),
        effective_timeout_s,
        effective_quiet,
    })
}

fn capture_pane_text(target: &PaneDispatchTarget, start_line: i32) -> Result<String> {
    match &target.backend {
        PaneBackend::Tmux => {
            let output = Command::new("tmux")
                .args([
                    "capture-pane",
                    "-p",
                    "-t",
                    target.pane_id.trim(),
                    "-S",
                    &format!("-{}", start_line.abs()),
                ])
                .output()
                .context("tmux capture-pane failed")?;
            if !output.status.success() {
                bail!(
                    "tmux capture-pane failed: pane={} status={}",
                    target.pane_id,
                    output.status
                );
            }
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        }
        PaneBackend::Wezterm { bin } => {
            let output = Command::new(bin)
                .args([
                    "cli",
                    "get-text",
                    "--pane-id",
                    target.pane_id.trim(),
                    "--start-line",
                    &format!("-{}", start_line.abs()),
                ])
                .output()
                .with_context(|| format!("wezterm get-text failed: bin={}", bin))?;
            if !output.status.success() {
                bail!(
                    "wezterm get-text failed: pane={} status={}",
                    target.pane_id,
                    output.status
                );
            }
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        }
    }
}

pub fn dispatch_text_to_pane(target: &PaneDispatchTarget, text: &str) -> Result<()> {
    let payload = text.replace('\r', "");
    match &target.backend {
        PaneBackend::Tmux => {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_else(|_| Duration::from_secs(0))
                .as_nanos();
            let buffer_name = format!("rccb-{}-{}", std::process::id(), nanos);
            let mut load = Command::new("tmux")
                .args(["load-buffer", "-b", &buffer_name, "-"])
                .stdin(Stdio::piped())
                .spawn()
                .context("tmux load-buffer spawn failed")?;
            if let Some(stdin) = load.stdin.as_mut() {
                stdin
                    .write_all(payload.as_bytes())
                    .context("tmux load-buffer write failed")?;
            }
            let status = load.wait().context("tmux load-buffer wait failed")?;
            if !status.success() {
                bail!("tmux load-buffer failed: status={}", status);
            }
            let paste_status = Command::new("tmux")
                .args([
                    "paste-buffer",
                    "-p",
                    "-t",
                    target.pane_id.trim(),
                    "-b",
                    &buffer_name,
                ])
                .status()
                .context("tmux paste-buffer failed")?;
            let _ = Command::new("tmux")
                .args(["delete-buffer", "-b", &buffer_name])
                .status();
            if !paste_status.success() {
                bail!("tmux paste-buffer failed: status={}", paste_status);
            }
            let enter_delay_ms = pane_enter_delay_ms("RCCB_TMUX_ENTER_DELAY_MS", 300);
            if enter_delay_ms > 0 {
                thread::sleep(Duration::from_millis(enter_delay_ms));
            }
            let enter_status = Command::new("tmux")
                .args(["send-keys", "-t", target.pane_id.trim(), "Enter"])
                .status()
                .context("tmux send-keys enter failed")?;
            if !enter_status.success() {
                bail!("tmux send-keys enter failed: status={}", enter_status);
            }
            Ok(())
        }
        PaneBackend::Wezterm { bin } => {
            let mut send = Command::new(bin)
                .args(["cli", "send-text", "--pane-id", target.pane_id.trim()])
                .stdin(Stdio::piped())
                .spawn()
                .with_context(|| format!("wezterm send-text spawn failed: bin={}", bin))?;
            if let Some(stdin) = send.stdin.as_mut() {
                stdin
                    .write_all(payload.as_bytes())
                    .context("wezterm send-text write failed")?;
            }
            let status = send.wait().context("wezterm send-text wait failed")?;
            if !status.success() {
                bail!("wezterm send-text failed: status={}", status);
            }
            let paste_delay_ms = pane_enter_delay_ms("RCCB_WEZTERM_PASTE_DELAY_MS", 120);
            if paste_delay_ms > 0 {
                thread::sleep(Duration::from_millis(paste_delay_ms));
            }
            send_wezterm_enter(bin, target.pane_id.trim())
        }
    }
}

fn send_wezterm_enter(bin: &str, pane_id: &str) -> Result<()> {
    for key in ["Enter", "Return"] {
        let status = Command::new(bin)
            .args(["cli", "send-key", "--pane-id", pane_id, "--key", key])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Ok(status) = status {
            if status.success() {
                return Ok(());
            }
        }

        let status = Command::new(bin)
            .args(["cli", "send-key", "--pane-id", pane_id, key])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Ok(status) = status {
            if status.success() {
                return Ok(());
            }
        }
    }

    let mut enter = Command::new(bin)
        .args(["cli", "send-text", "--pane-id", pane_id, "--no-paste"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("wezterm send-enter spawn failed: bin={}", bin))?;
    if let Some(stdin) = enter.stdin.as_mut() {
        stdin
            .write_all(b"\r")
            .context("wezterm send-enter write failed")?;
    }
    let enter_status = enter.wait().context("wezterm send-enter wait failed")?;
    if !enter_status.success() {
        bail!("wezterm send-enter failed: status={}", enter_status);
    }
    Ok(())
}

fn pane_enter_delay_ms(name: &str, default_ms: u64) -> u64 {
    match env::var(name) {
        Ok(raw) => raw.trim().parse::<u64>().unwrap_or(default_ms).min(5000),
        Err(_) => default_ms,
    }
}

fn pane_output_delta(previous: &str, current: &str) -> String {
    if current.is_empty() {
        return String::new();
    }
    if previous.is_empty() {
        return current.to_string();
    }
    if let Some(rest) = current.strip_prefix(previous) {
        return rest.to_string();
    }

    let prev_lines: Vec<&str> = previous.lines().collect();
    let curr_lines: Vec<&str> = current.lines().collect();
    if prev_lines.is_empty() {
        return current.to_string();
    }
    if curr_lines.is_empty() {
        return String::new();
    }

    let max_overlap = prev_lines.len().min(curr_lines.len());
    for overlap in (1..=max_overlap).rev() {
        if prev_lines[prev_lines.len() - overlap..] == curr_lines[..overlap] {
            return curr_lines[overlap..].join("\n");
        }
    }
    current.to_string()
}

fn pane_window_for_req(text: &str, req_id: &str) -> Option<String> {
    let marker = format!("{} {}", REQ_ID_PREFIX, req_id);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.iter().enumerate().rev().find_map(|(idx, line)| {
        if line.contains(&marker) {
            Some(idx)
        } else {
            None
        }
    })?;
    Some(lines[start..].join("\n"))
}

fn run_stub(req: &AskRequest, req_id: &str) -> ProviderExecResult {
    let reply = format!(
        "[rccb:stub] provider={} caller={} req_id={}\n{}\nRCCB_DONE: {}",
        req.provider, req.caller, req_id, req.message, req_id
    );
    ProviderExecResult {
        exit_code: 0,
        reply,
        done_seen: true,
        done_ms: Some(0),
        anchor_seen: true,
        anchor_ms: Some(0),
        fallback_scan: false,
        status: "completed".to_string(),
        stderr: String::new(),
        effective_timeout_s: req.timeout_s,
        effective_quiet: req.quiet,
    }
}

fn run_process_with_stream(
    mut cmd: Command,
    input: &str,
    timeout: Option<Duration>,
    on_delta: &mut dyn FnMut(String),
    should_cancel: &dyn Fn() -> bool,
) -> Result<ProcessOutcome> {
    let started = Instant::now();
    let mut child = cmd.spawn().context("spawn process failed")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input.as_bytes())
            .context("write process stdin failed")?;
        stdin.flush().context("flush process stdin failed")?;
    }

    let (tx, rx) = mpsc::channel::<PipeMsg>();
    spawn_pipe_reader(child.stdout.take(), tx.clone(), true);
    spawn_pipe_reader(child.stderr.take(), tx, false);

    let mut stdout_all = String::new();
    let mut stderr_all = String::new();
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let mut exit_code: Option<i32> = None;
    let mut timed_out = false;
    let mut canceled = false;

    loop {
        if should_cancel() {
            canceled = true;
            kill_child(&mut child);
            break;
        }

        if let Some(limit) = timeout {
            if started.elapsed() >= limit {
                timed_out = true;
                kill_child(&mut child);
                break;
            }
        }

        if exit_code.is_none() {
            if let Some(status) = child.try_wait().context("check process status failed")? {
                exit_code = Some(status.code().unwrap_or(1));
            }
        }

        match rx.recv_timeout(Duration::from_millis(40)) {
            Ok(PipeMsg::Stdout(chunk)) => {
                stdout_all.push_str(&chunk);
                on_delta(chunk);
            }
            Ok(PipeMsg::Stderr(chunk)) => {
                stderr_all.push_str(&chunk);
            }
            Ok(PipeMsg::StdoutEof) => stdout_eof = true,
            Ok(PipeMsg::StderrEof) => stderr_eof = true,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                stdout_eof = true;
                stderr_eof = true;
            }
        }

        if exit_code.is_some() && stdout_eof && stderr_eof {
            break;
        }
    }

    if exit_code.is_none() && !timed_out {
        let status = child.wait().context("wait process failed")?;
        exit_code = Some(status.code().unwrap_or(1));
    }

    Ok(ProcessOutcome {
        stdout: stdout_all,
        stderr: stderr_all,
        exit_code: if canceled {
            130
        } else if timed_out {
            2
        } else {
            exit_code.unwrap_or(1)
        },
        timed_out,
        canceled,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

fn build_exec_result(
    mode: ExecMode,
    req_id: &str,
    outcome: ProcessOutcome,
    effective_timeout_s: f64,
    effective_quiet: bool,
) -> ProviderExecResult {
    if outcome.canceled {
        return ProviderExecResult {
            exit_code: 130,
            reply: "请求已取消".to_string(),
            done_seen: false,
            done_ms: None,
            anchor_seen: true,
            anchor_ms: Some(0),
            fallback_scan: false,
            status: "canceled".to_string(),
            stderr: outcome.stderr,
            effective_timeout_s,
            effective_quiet,
        };
    }

    if outcome.timed_out {
        return ProviderExecResult {
            exit_code: 2,
            reply: "请求超时".to_string(),
            done_seen: false,
            done_ms: None,
            anchor_seen: true,
            anchor_ms: Some(0),
            fallback_scan: false,
            status: "timeout".to_string(),
            stderr: outcome.stderr,
            effective_timeout_s,
            effective_quiet,
        };
    }

    let done_seen_marker = contains_done_line_for_req(&outcome.stdout, req_id)
        || contains_done_line_for_req(&outcome.stderr, req_id);

    let reply_from_stdout = extract_reply_for_req(&outcome.stdout, req_id);
    let reply_from_stderr = extract_reply_for_req(&outcome.stderr, req_id);

    let mut reply = reply_from_stdout.trim().to_string();
    if reply.is_empty() {
        reply = sanitize_stderr_for_reply(&reply_from_stderr);
    }

    let (exit_code, done_seen, status) = match mode {
        ExecMode::Bridge => {
            let done = outcome.exit_code == 0 || done_seen_marker;
            let status = if outcome.exit_code == 0 {
                "completed"
            } else {
                "failed"
            };
            (outcome.exit_code, done, status)
        }
        ExecMode::Native => {
            if outcome.exit_code == 0 {
                (0, done_seen_marker, "completed")
            } else {
                (outcome.exit_code, done_seen_marker, "failed")
            }
        }
        ExecMode::Stub => (0, true, "completed"),
    };

    ProviderExecResult {
        exit_code,
        reply,
        done_seen,
        done_ms: if done_seen {
            Some(outcome.elapsed_ms)
        } else {
            None
        },
        anchor_seen: true,
        anchor_ms: Some(0),
        fallback_scan: false,
        status: status.to_string(),
        stderr: outcome.stderr,
        effective_timeout_s,
        effective_quiet,
    }
}

fn spawn_pipe_reader(
    pipe: Option<impl Read + Send + 'static>,
    tx: mpsc::Sender<PipeMsg>,
    stdout: bool,
) {
    thread::spawn(move || {
        if let Some(mut reader) = pipe {
            let mut buf = [0_u8; 4096];
            let mut pending_utf8 = Vec::<u8>::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunks = decode_utf8_chunks(&mut pending_utf8, &buf[..n]);
                        for chunk in chunks {
                            let _ = tx.send(if stdout {
                                PipeMsg::Stdout(chunk)
                            } else {
                                PipeMsg::Stderr(chunk)
                            });
                        }
                    }
                    Err(_) => break,
                }
            }

            if !pending_utf8.is_empty() {
                let tail = String::from_utf8_lossy(&pending_utf8).to_string();
                if !tail.is_empty() {
                    let _ = tx.send(if stdout {
                        PipeMsg::Stdout(tail)
                    } else {
                        PipeMsg::Stderr(tail)
                    });
                }
            }
        }

        let _ = tx.send(if stdout {
            PipeMsg::StdoutEof
        } else {
            PipeMsg::StderrEof
        });
    });
}

fn decode_utf8_chunks(pending: &mut Vec<u8>, incoming: &[u8]) -> Vec<String> {
    pending.extend_from_slice(incoming);
    let mut out = Vec::<String>::new();

    loop {
        if pending.is_empty() {
            break;
        }

        match std::str::from_utf8(pending) {
            Ok(all) => {
                if !all.is_empty() {
                    out.push(all.to_string());
                }
                pending.clear();
                break;
            }
            Err(err) => {
                let valid = err.valid_up_to();
                if valid > 0 {
                    let valid_txt = std::str::from_utf8(&pending[..valid]).unwrap_or_default();
                    if !valid_txt.is_empty() {
                        out.push(valid_txt.to_string());
                    }
                    pending.drain(..valid);
                }

                if let Some(err_len) = err.error_len() {
                    // Invalid UTF-8 byte sequence: emit replacement marker and drop invalid bytes.
                    out.push("\u{FFFD}".to_string());
                    let drop_n = err_len.min(pending.len());
                    pending.drain(..drop_n);
                    continue;
                }

                // Incomplete UTF-8 tail, wait for the next read chunk.
                break;
            }
        }
    }

    out
}

fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn resolve_wrapper_path(provider: &str) -> Result<PathBuf> {
    let provider = provider.trim().to_ascii_lowercase();
    let wrapper = wrapper_name(&provider)?;

    let env_key = format!("RCCB_{}_CMD", provider.to_ascii_uppercase());
    if let Ok(v) = env::var(&env_key) {
        let p = PathBuf::from(v.trim());
        if is_executable_path(&p) {
            return Ok(p);
        }
        bail!("{} is set but not executable: {}", env_key, p.display());
    }

    if let Ok(dir) = env::var("RCCB_BRIDGE_BIN_DIR") {
        let p = PathBuf::from(dir.trim()).join(wrapper);
        if is_executable_path(&p) {
            return Ok(p);
        }
    }

    if let Ok(root) = env::var("RCCB_BRIDGE_ROOT") {
        let p = PathBuf::from(root.trim()).join("bin").join(wrapper);
        if is_executable_path(&p) {
            return Ok(p);
        }
    }

    if let Some(path_cmd) = which(wrapper) {
        return Ok(path_cmd);
    }

    bail!(
        "wrapper `{}` 不存在，请检查 RCCB_{}_CMD / RCCB_BRIDGE_BIN_DIR / RCCB_BRIDGE_ROOT",
        wrapper,
        provider.to_ascii_uppercase()
    )
}

fn resolve_native_provider_cmd(
    provider: &str,
    work_dir: &Path,
    profile: Option<&NativeProviderProfile>,
) -> Result<PathBuf> {
    let provider = provider.trim().to_ascii_lowercase();
    let env_key = format!("RCCB_{}_NATIVE_CMD", provider.to_ascii_uppercase());
    if let Ok(v) = env::var(&env_key) {
        let p = resolve_cmd_path(v.trim(), work_dir);
        if is_executable_path(&p) {
            return Ok(p);
        }
        bail!("{} is set but not executable: {}", env_key, p.display());
    }

    if let Some(cmd) = profile
        .and_then(|p| p.cmd.as_ref())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        let p = resolve_cmd_path(cmd, work_dir);
        if is_executable_path(&p) {
            return Ok(p);
        }
        bail!(
            "native profile command is set but not executable: {}",
            p.display()
        );
    }

    if let Ok(dir) = env::var("RCCB_NATIVE_BIN_DIR") {
        let p = resolve_cmd_path(dir.trim(), work_dir).join(native_bin_name(&provider)?);
        if is_executable_path(&p) {
            return Ok(p);
        }
    }

    let bin = native_bin_name(&provider)?;

    // Prefer project-local binding to avoid relying on global installations.
    for p in [
        work_dir.join(".rccb").join("bin").join(bin),
        work_dir.join("bin").join(bin),
    ] {
        if is_executable_path(&p) {
            return Ok(p);
        }
    }

    if let Some(p) = which(bin) {
        return Ok(p);
    }

    bail!("native binary `{}` not found", bin)
}

fn resolve_cmd_path(raw: &str, work_dir: &Path) -> PathBuf {
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        p
    } else {
        work_dir.join(p)
    }
}

fn wrapper_name(provider: &str) -> Result<&'static str> {
    match provider {
        "codex" => Ok("cask"),
        "gemini" => Ok("gask"),
        "opencode" => Ok("oask"),
        "droid" => Ok("dask"),
        "claude" => Ok("lask"),
        other => bail!("unsupported provider for wrapper mode: {}", other),
    }
}

fn native_bin_name(provider: &str) -> Result<&'static str> {
    match provider {
        "codex" => Ok("codex"),
        "gemini" => Ok("gemini"),
        "opencode" => Ok("opencode"),
        "droid" => Ok("droid"),
        "claude" => Ok("claude"),
        other => bail!("unsupported provider for native mode: {}", other),
    }
}

fn native_args_for_provider(
    provider: &str,
    req: &AskRequest,
    req_id: &str,
    effective_timeout_s: f64,
    profile: Option<&NativeProviderProfile>,
) -> (Vec<String>, bool) {
    let key = format!("RCCB_{}_NATIVE_ARGS", provider.to_ascii_uppercase());
    if let Ok(v) = env::var(&key) {
        let parsed = split_shell_like(&v);
        if !parsed.is_empty() {
            return (
                apply_arg_templates(parsed, provider, req, req_id, effective_timeout_s),
                false,
            );
        }
    }

    if let Ok(v) = env::var("RCCB_NATIVE_ARGS") {
        return (
            apply_arg_templates(
                split_shell_like(&v),
                provider,
                req,
                req_id,
                effective_timeout_s,
            ),
            false,
        );
    }

    if let Some(args) = profile.and_then(|p| p.args.as_ref()) {
        return (
            apply_arg_templates(args.clone(), provider, req, req_id, effective_timeout_s),
            false,
        );
    }

    (
        apply_arg_templates(
            default_native_args_for_provider(provider),
            provider,
            req,
            req_id,
            effective_timeout_s,
        ),
        true,
    )
}

fn native_env_for_provider(
    provider: &str,
    req: &AskRequest,
    req_id: &str,
    effective_timeout_s: f64,
    profile: Option<&NativeProviderProfile>,
) -> Vec<(String, String)> {
    let Some(envs) = profile.and_then(|p| p.env.as_ref()) else {
        return Vec::new();
    };

    envs.iter()
        .filter_map(|(k, v)| {
            let key = k.trim();
            if key.is_empty() {
                return None;
            }
            Some((
                key.to_string(),
                render_arg_template(v, provider, req, req_id, effective_timeout_s),
            ))
        })
        .collect()
}

fn effective_native_timeout_s(
    provider: &str,
    request_timeout_s: f64,
    profile: Option<&NativeProviderProfile>,
) -> f64 {
    let provider_key = format!("RCCB_{}_NATIVE_TIMEOUT_S", provider.to_ascii_uppercase());
    if let Some(v) = parse_env_f64(&provider_key) {
        return v;
    }
    if let Some(v) = parse_env_f64("RCCB_NATIVE_TIMEOUT_S") {
        return v;
    }
    if let Some(v) = profile.and_then(|p| p.timeout_s) {
        return v;
    }
    request_timeout_s
}

fn effective_native_quiet(
    provider: &str,
    request_quiet: bool,
    profile: Option<&NativeProviderProfile>,
) -> bool {
    let provider_key = format!("RCCB_{}_NATIVE_QUIET", provider.to_ascii_uppercase());
    if let Some(v) = parse_env_bool(&provider_key) {
        return v;
    }
    if let Some(v) = parse_env_bool("RCCB_NATIVE_QUIET") {
        return v;
    }
    if let Some(v) = profile.and_then(|p| p.quiet) {
        return v;
    }
    request_quiet
}

fn should_wrap_native_prompt(provider: &str, profile: Option<&NativeProviderProfile>) -> bool {
    let provider_wrap_key = format!("RCCB_{}_NATIVE_WRAP", provider.to_ascii_uppercase());
    if env_bool(&provider_wrap_key, false) {
        return true;
    }
    if env_bool("RCCB_NATIVE_WRAP", false) {
        return true;
    }

    let provider_key = format!("RCCB_{}_NATIVE_NO_WRAP", provider.to_ascii_uppercase());
    if env_bool(&provider_key, false) {
        return false;
    }
    if env_bool("RCCB_NATIVE_NO_WRAP", false) {
        return false;
    }
    !profile.and_then(|p| p.no_wrap).unwrap_or(true)
}

fn apply_arg_templates(
    args: Vec<String>,
    provider: &str,
    req: &AskRequest,
    req_id: &str,
    effective_timeout_s: f64,
) -> Vec<String> {
    args.into_iter()
        .map(|arg| render_arg_template(&arg, provider, req, req_id, effective_timeout_s))
        .collect()
}

fn render_arg_template(
    arg: &str,
    provider: &str,
    req: &AskRequest,
    req_id: &str,
    effective_timeout_s: f64,
) -> String {
    arg.replace("{req_id}", req_id)
        .replace("{caller}", req.caller.trim())
        .replace("{provider}", provider.trim())
        .replace("{timeout_s}", &format!("{:.3}", effective_timeout_s))
        .replace("{work_dir}", req.work_dir.trim())
        .replace("{message}", req.message.trim_end())
}

fn default_native_args_for_provider(provider: &str) -> Vec<String> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "codex" => vec!["exec".to_string()],
        "gemini" => vec!["--prompt".to_string(), "{message}".to_string()],
        "opencode" => vec!["run".to_string(), "{message}".to_string()],
        "claude" => vec!["--print".to_string(), "{message}".to_string()],
        _ => Vec::new(),
    }
}

fn native_should_use_stdin(provider: &str, used_default_args: bool) -> bool {
    let provider_key = format!("RCCB_{}_NATIVE_STDIN", provider.to_ascii_uppercase());
    if let Some(v) = parse_env_bool(&provider_key) {
        return v;
    }
    if let Some(v) = parse_env_bool("RCCB_NATIVE_STDIN") {
        return v;
    }
    if used_default_args {
        return matches!(
            provider.trim().to_ascii_lowercase().as_str(),
            "codex" | "droid"
        );
    }
    true
}

fn load_native_profile(provider: &str, work_dir: &Path) -> Result<Option<NativeProviderProfile>> {
    let path = native_profile_path(provider, work_dir);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("read native profile failed: {}", path.display()))?;
    let parsed: NativeProviderProfile = serde_json::from_str(&raw)
        .with_context(|| format!("parse native profile failed: {}", path.display()))?;
    Ok(Some(parsed))
}

fn native_profile_path(provider: &str, work_dir: &Path) -> PathBuf {
    work_dir
        .join(".rccb")
        .join("providers")
        .join(format!("{}.json", provider.trim().to_ascii_lowercase()))
}

fn split_shell_like(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for ch in raw.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }

        match ch {
            '\\' if !in_single => {
                escaped = true;
            }
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(ch),
        }
    }

    if !cur.is_empty() {
        out.push(cur);
    }

    out
}

fn wrap_prompt_for_provider(provider: &str, message: &str, req_id: &str) -> String {
    let body = message.trim_end();
    match provider.trim().to_ascii_lowercase().as_str() {
        "claude" => format!(
            "{REQ_ID_PREFIX} {req_id}\n\n{body}\n\n请严格按照以下格式回复：\n{BEGIN_PREFIX} {req_id}\n<回复内容>\n{DONE_PREFIX} {req_id}\n"
        ),
        "gemini" => format!(
            "{REQ_ID_PREFIX} {req_id}\n\n{body}\n\n请严格按照以下格式回复：\n{BEGIN_PREFIX} {req_id}\n<回复内容>\n{DONE_PREFIX} {req_id}\n"
        ),
        "codex" | "opencode" | "droid" => format!(
            "{REQ_ID_PREFIX} {req_id}\n\n{body}\n\n请严格按照以下格式回复：\n{BEGIN_PREFIX} {req_id}\n<回复内容>\n{DONE_PREFIX} {req_id}\n"
        ),
        _ => format!(
            "{REQ_ID_PREFIX} {req_id}\n\n{body}\n\n注意：\n- 请将下面这一行原样作为最后一行输出：\n{DONE_PREFIX} {req_id}\n"
        ),
    }
}

fn contains_done_line_for_req(text: &str, req_id: &str) -> bool {
    let target = format!("{} {}", DONE_PREFIX, req_id);
    text.lines().any(|line| line.trim() == target)
}

fn is_any_done_line(line: &str) -> bool {
    let t = line.trim();
    if !t.starts_with(DONE_PREFIX) {
        return false;
    }
    let rest = t.trim_start_matches(DONE_PREFIX).trim();
    !rest.is_empty()
}

fn is_begin_line_for_req(line: &str, req_id: &str) -> bool {
    line.trim() == format!("{} {}", BEGIN_PREFIX, req_id)
}

fn extract_reply_for_req(text: &str, req_id: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    if lines.is_empty() {
        return String::new();
    }

    let target_done = format!("{} {}", DONE_PREFIX, req_id);
    let done_idxs: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, ln)| if is_any_done_line(ln) { Some(i) } else { None })
        .collect();

    let target_idxs: Vec<usize> = done_idxs
        .iter()
        .copied()
        .filter(|i| lines[*i].trim() == target_done)
        .collect();

    if target_idxs.is_empty() {
        if !done_idxs.is_empty() {
            // If there are done markers but none for this req_id, avoid mixing stale replies.
            return String::new();
        }
        return strip_done_text(text, req_id);
    }

    let target_i = *target_idxs.last().unwrap_or(&0);
    let mut start_i = 0usize;

    if let Some(prev_done) = done_idxs.iter().rev().find(|i| **i < target_i) {
        start_i = *prev_done + 1;
    }

    for i in (start_i..target_i).rev() {
        if is_begin_line_for_req(&lines[i], req_id) {
            start_i = i + 1;
            break;
        }
    }

    lines = lines[start_i..target_i].to_vec();
    trim_blank_lines(&mut lines);
    lines.join("\n").trim_end().to_string()
}

fn strip_done_text(text: &str, req_id: &str) -> String {
    let marker = format!("{} {}", DONE_PREFIX, req_id);
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    while let Some(last) = lines.last() {
        let t = last.trim();
        if t.is_empty() || is_trailing_noise_line(t) {
            lines.pop();
            continue;
        }
        if t == marker {
            lines.pop();
            continue;
        }
        break;
    }

    trim_blank_lines(&mut lines);
    lines.join("\n").trim_end().to_string()
}

fn trim_blank_lines(lines: &mut Vec<String>) {
    while let Some(first) = lines.first() {
        if first.trim().is_empty() {
            lines.remove(0);
        } else {
            break;
        }
    }

    while let Some(last) = lines.last() {
        if last.trim().is_empty() {
            lines.pop();
        } else {
            break;
        }
    }
}

fn is_trailing_noise_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return true;
    }
    if t.starts_with(DONE_PREFIX) {
        return false;
    }

    if !t.contains("_DONE") {
        return false;
    }

    t.chars().all(|c| {
        c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == ':' || c == '-' || c == ' '
    })
}

fn sanitize_stderr_for_reply(stderr: &str) -> String {
    let mut out = Vec::new();
    for line in stderr.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.starts_with("[RCCB_ASYNC_SUBMITTED") {
            continue;
        }
        out.push(line);
    }
    out.join("\n").trim().to_string()
}

fn parse_env_f64(name: &str) -> Option<f64> {
    let raw = env::var(name).ok()?;
    let parsed = raw.trim().parse::<f64>().ok()?;
    if !parsed.is_finite() {
        return None;
    }
    Some(parsed)
}

fn parse_env_bool(name: &str) -> Option<bool> {
    let raw = env::var(name).ok()?;
    let val = raw.trim().to_ascii_lowercase();
    if val.is_empty() {
        return None;
    }
    Some(!matches!(val.as_str(), "0" | "false" | "no" | "off"))
}

fn env_bool(name: &str, default: bool) -> bool {
    let raw = env::var(name).unwrap_or_default();
    let val = raw.trim().to_ascii_lowercase();
    if val.is_empty() {
        return default;
    }
    !matches!(val.as_str(), "0" | "false" | "no" | "off")
}

fn is_executable_path(path: &Path) -> bool {
    if !path.exists() || !path.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            return meta.permissions().mode() & 0o111 != 0;
        }
    }

    #[cfg(not(unix))]
    {
        return true;
    }

    false
}

fn which(cmd: &str) -> Option<PathBuf> {
    if cmd.contains(std::path::MAIN_SEPARATOR) {
        let p = PathBuf::from(cmd);
        if is_executable_path(&p) {
            return Some(p);
        }
        return None;
    }

    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(cmd);
        if is_executable_path(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::constants::{PROTOCOL_PREFIX, PROTOCOL_VERSION};
    use crate::types::AskRequest;

    fn sample_request() -> AskRequest {
        AskRequest {
            msg_type: format!("{}.request", PROTOCOL_PREFIX),
            v: PROTOCOL_VERSION,
            id: "id-1".to_string(),
            token: "token-1".to_string(),
            provider: "codex".to_string(),
            work_dir: ".".to_string(),
            timeout_s: 10.0,
            quiet: false,
            stream: false,
            async_mode: false,
            message: "hello".to_string(),
            caller: "claude".to_string(),
            req_id: None,
        }
    }

    #[test]
    fn split_shell_like_handles_quotes_and_escape() {
        let raw = r#"--model "gpt-5 codex" --flag 'a b' --path foo\ bar"#;
        let args = split_shell_like(raw);
        assert_eq!(
            args,
            vec![
                "--model".to_string(),
                "gpt-5 codex".to_string(),
                "--flag".to_string(),
                "a b".to_string(),
                "--path".to_string(),
                "foo bar".to_string()
            ]
        );
    }

    #[test]
    fn rccb_autostart_env_keys_cover_all_wrappers() {
        assert!(RCCB_AUTOSTART_ENV_KEYS.contains(&"RCCB_CASKD_AUTOSTART"));
        assert!(RCCB_AUTOSTART_ENV_KEYS.contains(&"RCCB_GASKD_AUTOSTART"));
        assert!(RCCB_AUTOSTART_ENV_KEYS.contains(&"RCCB_OASKD_AUTOSTART"));
        assert!(RCCB_AUTOSTART_ENV_KEYS.contains(&"RCCB_LASKD_AUTOSTART"));
        assert!(RCCB_AUTOSTART_ENV_KEYS.contains(&"RCCB_DASKD_AUTOSTART"));
    }

    #[test]
    fn default_native_args_match_headless_modes() {
        assert_eq!(
            default_native_args_for_provider("opencode"),
            vec!["run", "{message}"]
        );
        assert_eq!(
            default_native_args_for_provider("gemini"),
            vec!["--prompt", "{message}"]
        );
        assert_eq!(
            default_native_args_for_provider("claude"),
            vec!["--print", "{message}"]
        );
    }

    #[test]
    fn native_should_use_pane_exec_defaults_for_codex_and_opencode() {
        let old_global = std::env::var("RCCB_PANE_EXEC").ok();
        let old_codex = std::env::var("RCCB_CODEX_PANE_EXEC").ok();
        let old_open = std::env::var("RCCB_OPENCODE_PANE_EXEC").ok();
        unsafe {
            std::env::remove_var("RCCB_PANE_EXEC");
            std::env::remove_var("RCCB_CODEX_PANE_EXEC");
            std::env::remove_var("RCCB_OPENCODE_PANE_EXEC");
            std::env::remove_var("RCCB_GEMINI_PANE_EXEC");
            std::env::remove_var("RCCB_DROID_PANE_EXEC");
        }

        assert!(native_should_use_pane_exec("codex"));
        assert!(native_should_use_pane_exec("opencode"));
        assert!(native_should_use_pane_exec("gemini"));
        assert!(native_should_use_pane_exec("droid"));

        if let Some(v) = old_global {
            unsafe {
                std::env::set_var("RCCB_PANE_EXEC", v);
            }
        } else {
            unsafe {
                std::env::remove_var("RCCB_PANE_EXEC");
            }
        }
        if let Some(v) = old_codex {
            unsafe {
                std::env::set_var("RCCB_CODEX_PANE_EXEC", v);
            }
        } else {
            unsafe {
                std::env::remove_var("RCCB_CODEX_PANE_EXEC");
            }
        }
        if let Some(v) = old_open {
            unsafe {
                std::env::set_var("RCCB_OPENCODE_PANE_EXEC", v);
            }
        } else {
            unsafe {
                std::env::remove_var("RCCB_OPENCODE_PANE_EXEC");
            }
        }
    }

    #[test]
    fn extract_reply_prefers_latest_done_for_req() {
        let req_id = "req-777";
        let raw = format!(
            "noise\nRCCB_DONE: old-req\n{} {}\nreal answer line 1\nreal answer line 2\n{} {}\n",
            BEGIN_PREFIX, req_id, DONE_PREFIX, req_id
        );
        let got = extract_reply_for_req(&raw, req_id);
        assert_eq!(got, "real answer line 1\nreal answer line 2");
    }

    #[test]
    fn native_mode_without_done_marker_still_completes() {
        let req = sample_request();
        let req_id = "req-native";
        let outcome = ProcessOutcome {
            stdout: "plain reply without marker".to_string(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
            canceled: false,
            elapsed_ms: 15,
        };
        let result = build_exec_result(ExecMode::Native, req_id, outcome, req.timeout_s, req.quiet);
        assert_eq!(result.exit_code, 0);
        assert!(!result.done_seen);
        assert_eq!(result.status, "completed");
    }

    #[test]
    fn native_mode_accepts_done_marker_on_success_exit() {
        let req = sample_request();
        let req_id = "req-native-ok";
        let outcome = ProcessOutcome {
            stdout: format!("answer\n{} {}", DONE_PREFIX, req_id),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
            canceled: false,
            elapsed_ms: 21,
        };
        let result = build_exec_result(ExecMode::Native, req_id, outcome, req.timeout_s, req.quiet);
        assert_eq!(result.exit_code, 0);
        assert!(result.done_seen);
        assert_eq!(result.status, "completed");
        assert_eq!(result.reply, "answer");
    }

    #[test]
    fn extract_reply_returns_empty_when_only_other_req_done_exists() {
        let req_id = "req-current";
        let raw = "old reply\nRCCB_DONE: req-old\n";
        let got = extract_reply_for_req(raw, req_id);
        assert!(got.is_empty());
    }

    #[test]
    fn pane_window_for_req_uses_latest_req_marker() {
        let req_id = "req-current";
        let raw = format!(
            "banner\nRCCB_REQ_ID: old-req\nold body\nRCCB_DONE: old-req\nnoise\n> RCCB_REQ_ID: {req_id}\nIMPORTANT\nRCCB_BEGIN: {req_id}\nstep 1\nRCCB_DONE: {req_id}\n"
        );
        let got = pane_window_for_req(&raw, req_id).expect("window");
        assert!(got.starts_with(&format!("> RCCB_REQ_ID: {req_id}")));
        assert!(!got.contains("banner"));
        assert!(!got.contains("old body"));
    }

    #[test]
    fn codex_prompt_uses_begin_and_done_markers() {
        let prompt = wrap_prompt_for_provider("codex", "hello", "req-123");
        assert!(prompt.contains("RCCB_BEGIN: req-123"));
        assert!(prompt.contains("RCCB_DONE: req-123"));
        assert!(prompt.contains("请严格按照以下格式回复"));
    }

    #[test]
    fn gemini_prompt_uses_begin_and_done_markers() {
        let prompt = wrap_prompt_for_provider("gemini", "hello", "req-123");
        assert!(prompt.contains("RCCB_BEGIN: req-123"));
        assert!(prompt.contains("RCCB_DONE: req-123"));
        assert!(prompt.contains("请严格按照以下格式回复"));
    }

    #[test]
    fn extract_reply_for_req_ignores_pane_prompt_when_begin_exists() {
        let req_id = "req-pane";
        let raw = format!(
            "› RCCB_REQ_ID: {req_id}\n\nhello\n\n请严格按照以下格式回复：\nRCCB_BEGIN: {req_id}\nstep 1\nstep 2\nRCCB_DONE: {req_id}\n"
        );
        let got = extract_reply_for_req(&raw, req_id);
        assert_eq!(got, "step 1\nstep 2");
    }

    #[test]
    fn resolve_cmd_path_joins_relative_to_work_dir() {
        let work_dir = Path::new("/tmp/rccb-work");
        let p = resolve_cmd_path("tools/codex", work_dir);
        assert_eq!(p, work_dir.join("tools/codex"));
    }

    #[test]
    fn render_arg_template_substitutes_runtime_fields() {
        let req = sample_request();
        let got = render_arg_template(
            "--rid={req_id} --caller={caller} --p={provider} --wd={work_dir}",
            "codex",
            &req,
            "rid-123",
            req.timeout_s,
        );
        assert_eq!(got, "--rid=rid-123 --caller=claude --p=codex --wd=.");
    }

    #[test]
    fn native_profile_path_uses_project_local_layout() {
        let p = native_profile_path("Codex", Path::new("/tmp/myproj"));
        assert_eq!(p, Path::new("/tmp/myproj/.rccb/providers/codex.json"));
    }

    #[test]
    fn load_native_profile_reads_json_fields() {
        let uniq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rccb-prof-{}", uniq));
        let profile_dir = root.join(".rccb/providers");
        fs::create_dir_all(&profile_dir).expect("create profile dir");
        let path = profile_dir.join("codex.json");
        fs::write(
            &path,
            r#"{"cmd":"./bin/codex","args":["--rid={req_id}"],"no_wrap":true,"env":{"RCCB_MARK":"{provider}:{caller}"}}"#,
        )
        .expect("write profile");

        let profile = load_native_profile("codex", &root)
            .expect("load profile")
            .expect("profile present");

        assert_eq!(profile.cmd.as_deref(), Some("./bin/codex"));
        assert_eq!(
            profile.args.unwrap_or_default(),
            vec!["--rid={req_id}".to_string()]
        );
        assert_eq!(profile.no_wrap, Some(true));
        assert_eq!(
            profile
                .env
                .unwrap_or_default()
                .get("RCCB_MARK")
                .cloned()
                .unwrap_or_default(),
            "{provider}:{caller}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn native_env_for_provider_renders_templates() {
        let req = sample_request();
        let mut env_map = BTreeMap::new();
        env_map.insert("RCCB_MARK".to_string(), "{provider}:{caller}".to_string());
        let profile = NativeProviderProfile {
            cmd: None,
            args: None,
            no_wrap: None,
            env: Some(env_map),
            timeout_s: None,
            quiet: None,
        };
        let envs = native_env_for_provider("codex", &req, "rid-x", req.timeout_s, Some(&profile));
        assert_eq!(
            envs,
            vec![("RCCB_MARK".to_string(), "codex:claude".to_string())]
        );
    }

    #[test]
    fn effective_native_timeout_uses_profile_when_no_env_override() {
        let profile = NativeProviderProfile {
            timeout_s: Some(42.5),
            ..Default::default()
        };
        let got = effective_native_timeout_s("codex", 300.0, Some(&profile));
        assert!((got - 42.5).abs() < f64::EPSILON);
    }

    #[test]
    fn effective_native_quiet_uses_profile_when_no_env_override() {
        let profile = NativeProviderProfile {
            quiet: Some(true),
            ..Default::default()
        };
        let got = effective_native_quiet("codex", false, Some(&profile));
        assert!(got);
    }

    #[test]
    fn decode_utf8_chunks_keeps_multibyte_boundary() {
        let mut pending = Vec::new();
        let all = "中".as_bytes();
        let part1 = &all[..2];
        let part2 = &all[2..];

        let out1 = decode_utf8_chunks(&mut pending, part1);
        assert!(out1.is_empty());
        assert!(!pending.is_empty());

        let out2 = decode_utf8_chunks(&mut pending, part2);
        assert_eq!(out2, vec!["中".to_string()]);
        assert!(pending.is_empty());
    }

    #[test]
    fn decode_utf8_chunks_replaces_invalid_sequences() {
        let mut pending = Vec::new();
        let out = decode_utf8_chunks(&mut pending, &[0xff, b'a']);
        assert_eq!(out, vec!["\u{FFFD}".to_string(), "a".to_string()]);
        assert!(pending.is_empty());
    }
}

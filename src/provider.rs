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

const REQ_ID_PREFIX: &str = "CCB_REQ_ID:";
const BEGIN_PREFIX: &str = "CCB_BEGIN:";
const DONE_PREFIX: &str = "CCB_DONE:";

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecMode {
    Ccb,
    Native,
    Stub,
}

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
}

pub fn execute_provider_request(
    req: &AskRequest,
    req_id: &str,
    mut on_delta: impl FnMut(String),
) -> Result<ProviderExecResult> {
    let mode = execution_mode();
    match mode {
        ExecMode::Stub => Ok(run_stub(req, req_id)),
        ExecMode::Ccb => {
            let wrapper = resolve_wrapper_path(&req.provider).with_context(|| {
                format!(
                    "provider `{}` wrapper not found. set RCCB_{}_CMD or RCCB_CCB_BIN_DIR",
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
                .env("CCB_CALLER", &req.caller)
                .env("CCB_REQ_ID", req_id);
            if req.quiet {
                cmd.arg("--quiet");
            }
            if req.timeout_s >= 0.0 {
                cmd.arg("--timeout").arg(format!("{:.3}", req.timeout_s));
                cmd.env("CCB_SYNC_TIMEOUT", format!("{:.3}", req.timeout_s));
            }

            let input = format!("{}\n", req.message);
            let timeout = timeout_for_request(req.timeout_s);
            let outcome = run_process_with_stream(cmd, &input, timeout, &mut on_delta)
                .with_context(|| {
                    format!(
                        "spawn wrapper failed for provider={} wrapper={}",
                        req.provider,
                        wrapper.display()
                    )
                })?;
            Ok(build_exec_result(mode, req, req_id, outcome))
        }
        ExecMode::Native => {
            let work_dir = Path::new(&req.work_dir);
            let profile = load_native_profile(&req.provider, work_dir).with_context(|| {
                format!("load native provider profile failed for `{}`", req.provider)
            })?;

            let binary = resolve_native_provider_cmd(&req.provider, work_dir, profile.as_ref())
                .with_context(|| {
                    format!(
                        "provider `{}` native command not found. set RCCB_{}_NATIVE_CMD",
                        req.provider,
                        req.provider.to_ascii_uppercase()
                    )
                })?;
            let prompt = if should_wrap_native_prompt(&req.provider, profile.as_ref()) {
                wrap_prompt_for_provider(&req.provider, &req.message, req_id)
            } else {
                req.message.trim_end().to_string()
            };

            let mut cmd = Command::new(&binary);
            cmd.current_dir(&req.work_dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .env("CCB_CALLER", &req.caller)
                .env("CCB_REQ_ID", req_id)
                .env("RCCB_NATIVE_PROVIDER", &req.provider);

            for arg in native_args_for_provider(&req.provider, req, req_id, profile.as_ref()) {
                cmd.arg(arg);
            }
            for (k, v) in native_env_for_provider(&req.provider, req, req_id, profile.as_ref()) {
                cmd.env(k, v);
            }

            let input = format!("{}\n", prompt);
            let timeout = timeout_for_request(req.timeout_s);
            let outcome = run_process_with_stream(cmd, &input, timeout, &mut on_delta)
                .with_context(|| {
                    format!(
                        "spawn native provider failed: provider={} cmd={}",
                        req.provider,
                        binary.display()
                    )
                })?;

            Ok(build_exec_result(mode, req, req_id, outcome))
        }
    }
}

fn execution_mode() -> ExecMode {
    let raw = env::var("RCCB_EXEC_MODE").unwrap_or_else(|_| "ccb".to_string());
    match raw.trim().to_ascii_lowercase().as_str() {
        "stub" => ExecMode::Stub,
        "native" => ExecMode::Native,
        _ => ExecMode::Ccb,
    }
}

fn timeout_for_request(timeout_s: f64) -> Option<Duration> {
    if timeout_s < 0.0 {
        None
    } else {
        Some(Duration::from_secs_f64(timeout_s.max(0.1) + 5.0))
    }
}

fn run_stub(req: &AskRequest, req_id: &str) -> ProviderExecResult {
    let reply = format!(
        "[rccb:stub] provider={} caller={} req_id={}\n{}\nCCB_DONE: {}",
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
    }
}

fn run_process_with_stream(
    mut cmd: Command,
    input: &str,
    timeout: Option<Duration>,
    on_delta: &mut dyn FnMut(String),
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

    loop {
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
        exit_code: if timed_out { 2 } else { exit_code.unwrap_or(1) },
        timed_out,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

fn build_exec_result(
    mode: ExecMode,
    _req: &AskRequest,
    req_id: &str,
    outcome: ProcessOutcome,
) -> ProviderExecResult {
    if outcome.timed_out {
        return ProviderExecResult {
            exit_code: 2,
            reply: "request timeout".to_string(),
            done_seen: false,
            done_ms: None,
            anchor_seen: true,
            anchor_ms: Some(0),
            fallback_scan: false,
            status: "timeout".to_string(),
            stderr: outcome.stderr,
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
        ExecMode::Ccb => {
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
                if done_seen_marker {
                    (0, true, "completed")
                } else {
                    // Native mode enforces done-marker to guarantee deterministic completion.
                    (2, false, "incomplete")
                }
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
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                        let _ = tx.send(if stdout {
                            PipeMsg::Stdout(chunk)
                        } else {
                            PipeMsg::Stderr(chunk)
                        });
                    }
                    Err(_) => break,
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

    if let Ok(dir) = env::var("RCCB_CCB_BIN_DIR") {
        let p = PathBuf::from(dir.trim()).join(wrapper);
        if is_executable_path(&p) {
            return Ok(p);
        }
    }

    if let Ok(root) = env::var("RCCB_CCB_ROOT") {
        let p = PathBuf::from(root.trim()).join("bin").join(wrapper);
        if is_executable_path(&p) {
            return Ok(p);
        }
    }

    if let Some(path_cmd) = which(wrapper) {
        return Ok(path_cmd);
    }

    let local_repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("claude_code_bridge").join("bin").join(wrapper));
    if let Some(p) = local_repo {
        if is_executable_path(&p) {
            return Ok(p);
        }
    }

    bail!("wrapper `{}` not found in env/path/local ccb repo", wrapper)
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
    profile: Option<&NativeProviderProfile>,
) -> Vec<String> {
    let key = format!("RCCB_{}_NATIVE_ARGS", provider.to_ascii_uppercase());
    if let Ok(v) = env::var(&key) {
        let parsed = split_shell_like(&v);
        if !parsed.is_empty() {
            return apply_arg_templates(parsed, provider, req, req_id);
        }
    }

    if let Ok(v) = env::var("RCCB_NATIVE_ARGS") {
        return apply_arg_templates(split_shell_like(&v), provider, req, req_id);
    }

    if let Some(args) = profile.and_then(|p| p.args.as_ref()) {
        return apply_arg_templates(args.clone(), provider, req, req_id);
    }

    Vec::new()
}

fn native_env_for_provider(
    provider: &str,
    req: &AskRequest,
    req_id: &str,
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
                render_arg_template(v, provider, req, req_id),
            ))
        })
        .collect()
}

fn should_wrap_native_prompt(provider: &str, profile: Option<&NativeProviderProfile>) -> bool {
    let provider_key = format!("RCCB_{}_NATIVE_NO_WRAP", provider.to_ascii_uppercase());
    if env_bool(&provider_key, false) {
        return false;
    }
    if env_bool("RCCB_NATIVE_NO_WRAP", false) {
        return false;
    }
    !profile.and_then(|p| p.no_wrap).unwrap_or(false)
}

fn apply_arg_templates(
    args: Vec<String>,
    provider: &str,
    req: &AskRequest,
    req_id: &str,
) -> Vec<String> {
    args.into_iter()
        .map(|arg| render_arg_template(&arg, provider, req, req_id))
        .collect()
}

fn render_arg_template(arg: &str, provider: &str, req: &AskRequest, req_id: &str) -> String {
    arg.replace("{req_id}", req_id)
        .replace("{caller}", req.caller.trim())
        .replace("{provider}", provider.trim())
        .replace("{timeout_s}", &format!("{:.3}", req.timeout_s))
        .replace("{work_dir}", req.work_dir.trim())
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
            "{REQ_ID_PREFIX} {req_id}\n\n{body}\n\nReply using exactly this format:\n{BEGIN_PREFIX} {req_id}\n<reply>\n{DONE_PREFIX} {req_id}\n"
        ),
        "gemini" => format!(
            "{REQ_ID_PREFIX} {req_id}\n\n{body}\n\nIMPORTANT - you MUST follow these rules:\n1. Reply in English with an execution summary.\n2. Your FINAL line MUST be exactly:\n{DONE_PREFIX} {req_id}\n"
        ),
        "codex" | "opencode" | "droid" => format!(
            "{REQ_ID_PREFIX} {req_id}\n\n{body}\n\nIMPORTANT:\n- Reply normally, in English.\n- End your reply with this exact final line:\n{DONE_PREFIX} {req_id}\n"
        ),
        _ => format!(
            "{REQ_ID_PREFIX} {req_id}\n\n{body}\n\nIMPORTANT:\n- End your reply with this exact final line:\n{DONE_PREFIX} {req_id}\n"
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
        if t.starts_with("[CCB_ASYNC_SUBMITTED") {
            continue;
        }
        out.push(line);
    }
    out.join("\n").trim().to_string()
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
    fn extract_reply_prefers_latest_done_for_req() {
        let req_id = "req-777";
        let raw = format!(
            "noise\nCCB_DONE: old-req\n{} {}\nreal answer line 1\nreal answer line 2\n{} {}\n",
            BEGIN_PREFIX, req_id, DONE_PREFIX, req_id
        );
        let got = extract_reply_for_req(&raw, req_id);
        assert_eq!(got, "real answer line 1\nreal answer line 2");
    }

    #[test]
    fn native_mode_requires_done_marker_on_success_exit() {
        let req = sample_request();
        let req_id = "req-native";
        let outcome = ProcessOutcome {
            stdout: "plain reply without marker".to_string(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
            elapsed_ms: 15,
        };
        let result = build_exec_result(ExecMode::Native, &req, req_id, outcome);
        assert_eq!(result.exit_code, 2);
        assert!(!result.done_seen);
        assert_eq!(result.status, "incomplete");
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
            elapsed_ms: 21,
        };
        let result = build_exec_result(ExecMode::Native, &req, req_id, outcome);
        assert_eq!(result.exit_code, 0);
        assert!(result.done_seen);
        assert_eq!(result.status, "completed");
        assert_eq!(result.reply, "answer");
    }

    #[test]
    fn extract_reply_returns_empty_when_only_other_req_done_exists() {
        let req_id = "req-current";
        let raw = "old reply\nCCB_DONE: req-old\n";
        let got = extract_reply_for_req(raw, req_id);
        assert!(got.is_empty());
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
        };
        let envs = native_env_for_provider("codex", &req, "rid-x", Some(&profile));
        assert_eq!(
            envs,
            vec![("RCCB_MARK".to_string(), "codex:claude".to_string())]
        );
    }
}

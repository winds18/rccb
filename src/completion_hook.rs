use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::io_utils::write_line;

#[derive(Debug, Clone)]
pub struct CompletionHookInput {
    pub provider: String,
    pub caller: String,
    pub req_id: String,
    pub status: String,
    pub done_seen: bool,
    pub exit_code: i32,
    pub reply: String,
    pub instance_id: String,
    pub project_dir: String,
    pub work_dir: String,
    pub log_file: PathBuf,
}

pub fn notify_completion_async(input: CompletionHookInput) {
    if !env_bool("RCCB_COMPLETION_HOOK_ENABLED", true) {
        return;
    }

    let Some(raw_cmd) = resolve_hook_command(&input.provider) else {
        return;
    };
    let parts = split_shell_like(&raw_cmd);
    if parts.is_empty() {
        let _ = write_line(
            input.log_file.clone(),
            &format!(
                "[WARN] completion hook skipped: empty cmd provider={} req_id={}",
                input.provider, input.req_id
            ),
        );
        return;
    }

    let timeout = completion_hook_timeout();
    thread::spawn(move || run_hook(parts, timeout, input));
}

fn run_hook(parts: Vec<String>, timeout: Duration, input: CompletionHookInput) {
    let program = parts[0].clone();
    let args = parts[1..].to_vec();
    let normalized_status = normalize_status(&input.status, input.done_seen);

    let mut cmd = Command::new(&program);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .current_dir(&input.work_dir)
        .env("RCCB_HOOK_PROVIDER", &input.provider)
        .env("RCCB_HOOK_CALLER", &input.caller)
        .env("RCCB_HOOK_REQ_ID", &input.req_id)
        .env("RCCB_HOOK_STATUS", &normalized_status)
        .env(
            "RCCB_HOOK_DONE_SEEN",
            if input.done_seen { "1" } else { "0" },
        )
        .env("RCCB_HOOK_EXIT_CODE", input.exit_code.to_string())
        .env("RCCB_HOOK_INSTANCE_ID", &input.instance_id)
        .env("RCCB_HOOK_PROJECT_DIR", &input.project_dir)
        .env("RCCB_HOOK_WORK_DIR", &input.work_dir)
        .env("RCCB_CALLER", &input.caller)
        .env("RCCB_REQ_ID", &input.req_id)
        .env("RCCB_DONE_SEEN", if input.done_seen { "1" } else { "0" })
        .env("RCCB_COMPLETION_STATUS", &normalized_status);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            let _ = write_line(
                input.log_file.clone(),
                &format!(
                    "[WARN] completion hook spawn failed provider={} req_id={} cmd={} err={}",
                    input.provider, input.req_id, program, err
                ),
            );
            return;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.reply.as_bytes());
        let _ = stdin.flush();
    }

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let code = status.code().unwrap_or(1);
                let _ = write_line(
                    input.log_file.clone(),
                    &format!(
                        "[INFO] completion hook done provider={} req_id={} status={} exit_code={}",
                        input.provider, input.req_id, normalized_status, code
                    ),
                );
                return;
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = write_line(
                        input.log_file.clone(),
                        &format!(
                            "[WARN] completion hook timeout provider={} req_id={} timeout_ms={}",
                            input.provider,
                            input.req_id,
                            timeout.as_millis()
                        ),
                    );
                    return;
                }
                thread::sleep(Duration::from_millis(40));
            }
            Err(err) => {
                let _ = write_line(
                    input.log_file.clone(),
                    &format!(
                        "[WARN] completion hook wait failed provider={} req_id={} err={}",
                        input.provider, input.req_id, err
                    ),
                );
                return;
            }
        }
    }
}

fn resolve_hook_command(provider: &str) -> Option<String> {
    let provider_key = format!(
        "RCCB_{}_COMPLETION_HOOK_CMD",
        provider.trim().to_ascii_uppercase()
    );
    if let Ok(v) = env::var(&provider_key) {
        let t = v.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }

    if let Ok(v) = env::var("RCCB_COMPLETION_HOOK_CMD") {
        let t = v.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }

    None
}

fn completion_hook_timeout() -> Duration {
    let raw = env::var("RCCB_COMPLETION_HOOK_TIMEOUT_S")
        .unwrap_or_else(|_| "30".to_string())
        .trim()
        .to_string();

    match raw.parse::<f64>() {
        Ok(v) if v.is_finite() && v > 0.0 => Duration::from_secs_f64(v.min(300.0)),
        _ => Duration::from_secs(30),
    }
}

fn env_bool(name: &str, default: bool) -> bool {
    let raw = env::var(name).unwrap_or_default();
    let val = raw.trim().to_ascii_lowercase();
    if val.is_empty() {
        return default;
    }
    !matches!(val.as_str(), "0" | "false" | "no" | "off")
}

fn normalize_status(status: &str, done_seen: bool) -> &'static str {
    match status.trim().to_ascii_lowercase().as_str() {
        "completed" => "completed",
        "canceled" => "cancelled",
        "cancelled" => "cancelled",
        "timeout" => "cancelled",
        "failed" => "failed",
        "incomplete" => "incomplete",
        _ => {
            if done_seen {
                "completed"
            } else {
                "incomplete"
            }
        }
    }
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
            '\\' if !in_single => escaped = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
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

#[cfg(test)]
mod tests {
    use super::{normalize_status, split_shell_like};

    #[test]
    fn split_shell_like_handles_quotes() {
        let raw = r#"hook --name "rccb notify" --tag 'task done' --path foo\ bar"#;
        let args = split_shell_like(raw);
        assert_eq!(
            args,
            vec![
                "hook".to_string(),
                "--name".to_string(),
                "rccb notify".to_string(),
                "--tag".to_string(),
                "task done".to_string(),
                "--path".to_string(),
                "foo bar".to_string()
            ]
        );
    }

    #[test]
    fn normalize_status_maps_timeout_and_unknown() {
        assert_eq!(normalize_status("timeout", false), "cancelled");
        assert_eq!(normalize_status("canceled", false), "cancelled");
        assert_eq!(normalize_status("completed", false), "completed");
        assert_eq!(normalize_status("unknown", true), "completed");
        assert_eq!(normalize_status("unknown", false), "incomplete");
    }
}

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sysinfo::{Pid, System};

use crate::constants::{PROTOCOL_PREFIX, PROTOCOL_VERSION, SUPPORTED_PROVIDERS};
use crate::daemon::start_instance;
use crate::im::{FeishuChannel, ImChannel, TelegramChannel};
use crate::io_utils::{
    build_http_client, is_process_alive, load_all_states, load_state, normalize_provider,
    normalize_provider_list, now_unix, read_stdin_all, update_task_status, write_json_pretty,
};
use crate::layout::{
    ensure_project_layout, launcher_meta_path, logs_instance_dir, rccb_dir, sanitize_filename,
    session_instance_dir, state_path, tasks_instance_dir, tasks_root_dir, tmp_instance_dir,
};
use crate::protocol::{connect_and_send, send_wire_message};
use crate::provider::{
    dispatch_text_to_pane, PaneBackend as ProviderPaneBackend, PaneDispatchTarget,
};
use crate::types::{AskBusEvent, AskEvent, AskResponse, InstanceState};

pub fn cmd_init(project_dir: &Path, force: bool) -> Result<()> {
    ensure_project_layout(project_dir)?;
    let config_path = rccb_dir(project_dir).join("config.example.json");
    if !config_path.exists() || force {
        let template = json!({
            "project": project_dir.display().to_string(),
            "instances": {
                "default": {
                    "heartbeat_secs": 5,
                    "listen": "127.0.0.1:0",
                    "debug": false,
                    "providers": ["claude", "codex", "gemini", "opencode", "droid"],
                    "orchestration_rule": "first provider is orchestrator, remaining providers are executors"
                }
            },
            "channels": {
                "feishu": {
                    "webhook_url": "https://open.feishu.cn/open-apis/bot/v2/hook/your-token"
                },
                "telegram": {
                    "bot_token": "123456789:bot-token",
                    "chat_id": "-1001234567890"
                }
            }
        });
        write_json_pretty(&config_path, &template)?;
    }

    let profile_templates = write_native_profile_templates(project_dir, force)?;

    println!("初始化完成：{}", rccb_dir(project_dir).display());
    println!("配置模板：{}", config_path.display());
    for p in profile_templates {
        println!("native profile 模板：{}", p.display());
    }
    Ok(())
}

fn write_native_profile_templates(project_dir: &Path, force: bool) -> Result<Vec<PathBuf>> {
    let profile_dir = rccb_dir(project_dir).join("providers");
    fs::create_dir_all(&profile_dir)?;

    let mut written = Vec::new();
    for provider in SUPPORTED_PROVIDERS {
        let path = profile_dir.join(format!("{}.example.json", provider));
        if path.exists() && !force {
            continue;
        }

        let tpl = json!({
            "provider": provider,
            "cmd": format!("./.rccb/bin/{}", provider),
            "args": [],
            "timeout_s": 300.0,
            "quiet": false,
            "no_wrap": false,
            "env": {
                "RCCB_TASK_ID": "{req_id}",
                "RCCB_TASK_CALLER": "{caller}"
            },
            "_note": "copy to <provider>.json and customize cmd/args/timeout_s/quiet/no_wrap/env for this project"
        });
        write_json_pretty(&path, &tpl)?;
        written.push(path);
    }

    Ok(written)
}

pub fn cmd_start(
    project_dir: &Path,
    instance: &str,
    heartbeat_secs: u64,
    listen: &str,
    providers: Vec<String>,
    initial_task: Option<String>,
    debug: bool,
) -> Result<()> {
    let normalized = if providers.is_empty() {
        SUPPORTED_PROVIDERS.iter().map(|x| x.to_string()).collect()
    } else {
        normalize_provider_list(&providers)?
    };
    let effective_debug = resolve_start_debug(project_dir, instance, debug);

    start_instance(
        project_dir,
        instance,
        heartbeat_secs,
        listen,
        normalized,
        initial_task,
        effective_debug,
    )
}

pub fn cmd_external_provider_launch(project_dir: &Path, raw: Vec<String>) -> Result<()> {
    if raw.is_empty() {
        bail!("缺少 provider 参数。示例：rccb claude codex gemini opencode droid");
    }

    let op = raw[0].trim().to_ascii_lowercase();
    if let Some(provider) = legacy_ask_alias_provider(&op) {
        return cmd_legacy_ask_alias(project_dir, provider, raw[1..].to_vec());
    }
    if let Some(provider) = legacy_ping_alias_provider(&op) {
        return cmd_legacy_ping_alias(project_dir, provider);
    }
    if let Some(provider) = legacy_pend_alias_provider(&op) {
        return cmd_legacy_pend_alias(project_dir, provider);
    }

    if raw.iter().any(|x| x.starts_with('-')) {
        bail!(
            "快捷入口仅接受 provider 列表；如需参数请使用 `rccb start --instance <id> <providers...>`"
        );
    }

    let normalized = normalize_provider_list(&raw)?;
    if normalized.is_empty() {
        bail!("至少需要一个 provider");
    }
    let effective_debug = resolve_start_debug(project_dir, "default", env_debug_enabled());
    ensure_project_layout(project_dir)?;
    restart_default_daemon_for_shortcut(project_dir)?;
    ensure_default_daemon_running(project_dir, &normalized, effective_debug)?;

    if launch_provider_clis(project_dir, &normalized, effective_debug)? {
        return Ok(());
    }

    println!(
        "daemon 已就绪：project={} instance=default providers={}",
        project_dir.display(),
        normalized.join(",")
    );
    println!("未检测到终端后端（tmux/wezterm），未自动拉起 provider CLI pane。");
    println!(
        "你仍可通过以下方式发起请求：rccb --project-dir . ask --instance default --provider {} --caller {} \"...\"",
        normalized
            .first()
            .cloned()
            .unwrap_or_else(|| "codex".to_string()),
        normalized
            .first()
            .cloned()
            .unwrap_or_else(|| "manual".to_string())
    );
    Ok(())
}

fn legacy_ask_alias_provider(op: &str) -> Option<&'static str> {
    match op {
        "cask" => Some("codex"),
        "gask" => Some("gemini"),
        "oask" => Some("opencode"),
        "lask" => Some("claude"),
        "dask" => Some("droid"),
        _ => None,
    }
}

fn legacy_ping_alias_provider(op: &str) -> Option<&'static str> {
    match op {
        "cping" => Some("codex"),
        "gping" => Some("gemini"),
        "oping" => Some("opencode"),
        "lping" => Some("claude"),
        "dping" => Some("droid"),
        _ => None,
    }
}

fn legacy_pend_alias_provider(op: &str) -> Option<&'static str> {
    match op {
        "cpend" => Some("codex"),
        "gpend" => Some("gemini"),
        "opend" | "opend-pend" | "opend_pend" => Some("opencode"),
        "lpend" => Some("claude"),
        "dpend" => Some("droid"),
        _ => None,
    }
}

fn cmd_legacy_ask_alias(
    project_dir: &Path,
    provider: &str,
    message_parts: Vec<String>,
) -> Result<()> {
    let provider_vec = vec![provider.to_string()];
    ensure_default_daemon_running(
        project_dir,
        &provider_vec,
        resolve_start_debug(project_dir, "default", false),
    )?;

    cmd_ask(
        project_dir,
        "default",
        provider,
        "manual",
        300.0,
        false,
        false,
        false,
        None,
        message_parts,
    )
}

fn cmd_legacy_ping_alias(project_dir: &Path, provider: &str) -> Result<()> {
    let provider_vec = vec![provider.to_string()];
    ensure_default_daemon_running(
        project_dir,
        &provider_vec,
        resolve_start_debug(project_dir, "default", false),
    )?;
    cmd_ping(project_dir, "default", 1.0)?;
    println!("provider={} 已就绪", provider);
    Ok(())
}

fn cmd_legacy_pend_alias(project_dir: &Path, provider: &str) -> Result<()> {
    let tasks = load_tasks_in_instance(project_dir, "default")?;
    let mut latest: Option<TaskView> = None;
    for task in tasks {
        if task.provider.as_deref() != Some(provider) {
            continue;
        }
        if latest
            .as_ref()
            .map(|x| x.created_at_unix.unwrap_or(0) < task.created_at_unix.unwrap_or(0))
            .unwrap_or(true)
        {
            latest = Some(task);
        }
    }

    if let Some(task) = latest {
        if let Some(reply) = task.reply {
            if !reply.trim().is_empty() {
                println!("{}", reply);
                return Ok(());
            }
        }
        println!(
            "最近任务尚无回复：provider={} req_id={} status={}",
            provider,
            task.req_id.unwrap_or_else(|| "-".to_string()),
            task.status
        );
        return Ok(());
    }

    println!("未找到任务：provider={} instance=default", provider);
    Ok(())
}

fn ensure_default_daemon_running(
    project_dir: &Path,
    providers: &[String],
    debug_enabled: bool,
) -> Result<()> {
    if is_daemon_ready(project_dir, "default") {
        return Ok(());
    }

    let exe = env::current_exe().context("获取当前可执行文件路径失败")?;
    let launch_log = logs_instance_dir(project_dir, "default").join("daemon.launch.log");
    if let Some(parent) = launch_log.parent() {
        fs::create_dir_all(parent)?;
    }
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&launch_log)
        .with_context(|| format!("打开启动日志失败：{}", launch_log.display()))?;
    let stderr = stdout.try_clone().context("克隆启动日志句柄失败")?;

    let mut cmd = ProcessCommand::new(exe);
    cmd.arg("--project-dir")
        .arg(project_dir)
        .arg("start")
        .arg("--instance")
        .arg("default")
        .arg("--heartbeat-secs")
        .arg("5")
        .arg("--listen")
        .arg("127.0.0.1:0");
    if debug_enabled {
        cmd.arg("--debug");
    }
    for provider in providers {
        cmd.arg(provider);
    }
    cmd.stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .stdin(Stdio::null());

    let _child = cmd
        .spawn()
        .with_context(|| format!("启动 daemon 失败，请查看 {}", launch_log.display()))?;

    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if is_daemon_ready(project_dir, "default") {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(120));
    }

    bail!("daemon 启动超时，请检查日志：{}", launch_log.display())
}

fn restart_default_daemon_for_shortcut(project_dir: &Path) -> Result<()> {
    let instance = SHORTCUT_INSTANCE;
    let path = state_path(project_dir, instance);
    if path.exists() {
        let was_running = is_daemon_ready(project_dir, instance);
        if was_running {
            println!("检测到旧的 default 实例仍在运行，正在重启以应用最新规则...");
            let _ = cmd_stop(project_dir, instance);
        }
        let _ = cleanup_inflight_tasks(project_dir, instance);
        let _ = fs::remove_dir_all(tmp_instance_dir(project_dir, instance).join("launcher"));
    }
    Ok(())
}

fn is_daemon_ready(project_dir: &Path, instance: &str) -> bool {
    let path = state_path(project_dir, instance);
    if !path.exists() {
        return false;
    }
    let state = match load_state(&path) {
        Ok(v) => v,
        Err(_) => return false,
    };
    if state.status != "running" {
        return false;
    }
    if !is_process_alive(state.pid) {
        return false;
    }
    ping_daemon_state(&state, 0.8).is_ok()
}

fn ping_daemon_state(state: &InstanceState, timeout_s: f64) -> Result<()> {
    let host = state
        .daemon_host
        .clone()
        .ok_or_else(|| anyhow!("缺少 daemon_host"))?;
    let port = state
        .daemon_port
        .ok_or_else(|| anyhow!("缺少 daemon_port"))?;
    let token = state
        .daemon_token
        .clone()
        .ok_or_else(|| anyhow!("缺少 daemon_token"))?;

    let req = json!({
        "type": format!("{}.ping", PROTOCOL_PREFIX),
        "v": PROTOCOL_VERSION,
        "id": "probe",
        "token": token,
    });
    let resp = send_wire_message(&host, port, req, timeout_s)?;
    let msg_type = resp
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if msg_type != format!("{}.pong", PROTOCOL_PREFIX) {
        bail!("探活响应类型异常：{}", msg_type);
    }
    Ok(())
}

#[derive(Debug, Clone)]
enum LaunchBackend {
    Tmux { anchor_pane: String },
    Wezterm { anchor_pane: String, bin: String },
}

const SHORTCUT_INSTANCE: &str = "default";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LauncherProviderMeta {
    provider: String,
    role: String,
    feed_file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LauncherMeta {
    schema_version: u32,
    instance: String,
    created_at_unix: u64,
    backend: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    backend_bin: Option<String>,
    orchestrator: String,
    providers: Vec<LauncherProviderMeta>,
}

fn launch_provider_clis(
    project_dir: &Path,
    providers: &[String],
    debug_enabled: bool,
) -> Result<bool> {
    let Some(backend) = detect_launch_backend()? else {
        return Ok(false);
    };

    run_interactive_layout(project_dir, providers, backend, debug_enabled)?;
    Ok(true)
}

fn detect_launch_backend() -> Result<Option<LaunchBackend>> {
    let wez_pane = env::var("WEZTERM_PANE").unwrap_or_default();
    if !wez_pane.trim().is_empty() {
        return Ok(Some(LaunchBackend::Wezterm {
            anchor_pane: wez_pane.trim().to_string(),
            bin: env::var("RCCB_WEZTERM_BIN").unwrap_or_else(|_| "wezterm".to_string()),
        }));
    }

    let inside_tmux = env::var("TMUX")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
        || env::var("TMUX_PANE")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
    if !inside_tmux {
        return Ok(None);
    }

    let current_pane = run_capture(
        "tmux",
        &["display-message", "-p", "#{pane_id}"],
        "获取当前 tmux pane 失败",
    )?;
    let pane = current_pane.trim().to_string();
    if pane.is_empty() {
        bail!("无法解析当前 tmux pane id");
    }
    Ok(Some(LaunchBackend::Tmux { anchor_pane: pane }))
}

fn run_interactive_layout(
    project_dir: &Path,
    providers: &[String],
    backend: LaunchBackend,
    debug_enabled: bool,
) -> Result<()> {
    if providers.is_empty() {
        bail!("至少需要一个 provider");
    }

    let orchestrator = providers[0].clone();
    let (left_items, right_items) = split_layout_groups(&providers[1..]);
    let mut spawned_panes: Vec<String> = Vec::new();
    let mut provider_panes: HashMap<String, String> = HashMap::new();

    let run_result = match &backend {
        LaunchBackend::Tmux { anchor_pane } => {
            provider_panes.insert(orchestrator.clone(), anchor_pane.clone());
            mark_anchor_pane(&backend, anchor_pane, &orchestrator);
            spawn_tmux_layout(
                project_dir,
                SHORTCUT_INSTANCE,
                anchor_pane,
                &left_items,
                &right_items,
                &mut spawned_panes,
                &mut provider_panes,
            )?;
            if debug_enabled {
                if let Some(debug_pane) = maybe_spawn_debug_watch_pane(
                    project_dir,
                    SHORTCUT_INSTANCE,
                    providers,
                    &backend,
                    anchor_pane,
                )? {
                    spawned_panes.push(debug_pane);
                }
            }
            prepare_launcher_runtime(
                project_dir,
                SHORTCUT_INSTANCE,
                providers,
                &backend,
                &provider_panes,
            )?;
            maybe_prime_orchestrator_pane(&backend, anchor_pane, &orchestrator, &providers[1..]);
            ensure_orchestrator_focus(&backend, anchor_pane);
            run_orchestrator_foreground(project_dir, SHORTCUT_INSTANCE, &orchestrator)
        }
        LaunchBackend::Wezterm { anchor_pane, bin } => {
            provider_panes.insert(orchestrator.clone(), anchor_pane.clone());
            spawn_wezterm_layout(
                project_dir,
                SHORTCUT_INSTANCE,
                bin,
                anchor_pane,
                &left_items,
                &right_items,
                &mut spawned_panes,
                &mut provider_panes,
            )?;
            if debug_enabled {
                if let Some(debug_pane) = maybe_spawn_debug_watch_pane(
                    project_dir,
                    SHORTCUT_INSTANCE,
                    providers,
                    &backend,
                    anchor_pane,
                )? {
                    spawned_panes.push(debug_pane);
                }
            }
            prepare_launcher_runtime(
                project_dir,
                SHORTCUT_INSTANCE,
                providers,
                &backend,
                &provider_panes,
            )?;
            maybe_prime_orchestrator_pane(&backend, anchor_pane, &orchestrator, &providers[1..]);
            ensure_orchestrator_focus(&backend, anchor_pane);
            run_orchestrator_foreground(project_dir, SHORTCUT_INSTANCE, &orchestrator)
        }
    };

    if let Err(err) = cleanup_after_orchestrator(project_dir, &backend, &spawned_panes) {
        eprintln!("警告：清理失败：{}", err);
    }

    let code = run_result?;
    if code != 0 {
        bail!("编排者 `{}` 已退出，退出码 {}", orchestrator, code);
    }
    Ok(())
}

fn mark_anchor_pane(backend: &LaunchBackend, pane_id: &str, provider: &str) {
    if let LaunchBackend::Tmux { .. } = backend {
        let title = format!("RCCB-{}", provider);
        let _ = run_simple("tmux", &["select-pane", "-t", pane_id.trim(), "-T", &title]);
    }
}

fn ensure_orchestrator_focus(backend: &LaunchBackend, pane_id: &str) {
    let pane = pane_id.trim();
    if pane.is_empty() {
        return;
    }

    let result = match backend {
        LaunchBackend::Tmux { .. } => run_simple("tmux", &["select-pane", "-t", pane]),
        LaunchBackend::Wezterm { bin, .. } => {
            run_simple(bin, &["cli", "activate-pane", "--pane-id", pane])
        }
    };

    if let Err(err) = result {
        eprintln!("警告：无法聚焦编排者 pane={} err={}", pane, err);
    }
}

fn split_layout_groups(executors: &[String]) -> (Vec<String>, Vec<String>) {
    if executors.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // 保持 provider pane 启动布局稳定：
    // - provider 总数 <=4：左侧只保留编排者。
    // - provider 总数 =5：左侧拆成上下两个 pane。
    let total = executors.len() + 1;
    if total <= 4 {
        return (Vec::new(), executors.to_vec());
    }

    let left = vec![executors[0].clone()];
    let right = if executors.len() > 1 {
        executors[1..].to_vec()
    } else {
        Vec::new()
    };
    (left, right)
}

fn spawn_tmux_layout(
    project_dir: &Path,
    instance: &str,
    anchor_pane: &str,
    left_items: &[String],
    right_items: &[String],
    spawned_panes: &mut Vec<String>,
    provider_panes: &mut HashMap<String, String>,
) -> Result<()> {
    let mut right_remainder: Option<String> = None;
    if let Some(first) = right_items.first() {
        let pane = spawn_tmux_pane(project_dir, instance, anchor_pane, "right", first, Some(50))?;
        spawned_panes.push(pane.clone());
        provider_panes.insert(first.clone(), pane.clone());
        right_remainder = Some(pane);
    }

    let right_total = right_items.len();
    for (i, provider) in right_items.iter().enumerate().skip(1) {
        let parent = right_remainder.as_deref().unwrap_or(anchor_pane);
        let percent = split_percent_for_equal_stack(right_total, i);
        let pane = spawn_tmux_pane(
            project_dir,
            instance,
            parent,
            "bottom",
            provider,
            Some(percent),
        )?;
        spawned_panes.push(pane.clone());
        provider_panes.insert(provider.clone(), pane.clone());
        right_remainder = Some(pane);
    }

    let mut left_parent = anchor_pane.to_string();
    for provider in left_items {
        let pane = spawn_tmux_pane(
            project_dir,
            instance,
            &left_parent,
            "bottom",
            provider,
            Some(50),
        )?;
        spawned_panes.push(pane.clone());
        provider_panes.insert(provider.clone(), pane.clone());
        left_parent = pane;
    }

    Ok(())
}

fn spawn_tmux_pane(
    project_dir: &Path,
    instance: &str,
    parent: &str,
    direction: &str,
    provider: &str,
    percent: Option<u8>,
) -> Result<String> {
    let provider_cmd = provider_start_cmd(project_dir, instance, provider);
    let pane_id = spawn_tmux_custom_pane(
        parent,
        direction,
        percent,
        &provider_cmd,
        &format!("RCCB-{}", provider),
    )?;
    println!(
        "已拉起 provider CLI：provider={} backend=tmux pane={}",
        provider, pane_id
    );
    Ok(pane_id)
}

fn spawn_wezterm_layout(
    project_dir: &Path,
    instance: &str,
    wezterm_bin: &str,
    anchor_pane: &str,
    left_items: &[String],
    right_items: &[String],
    spawned_panes: &mut Vec<String>,
    provider_panes: &mut HashMap<String, String>,
) -> Result<()> {
    let mut right_remainder: Option<String> = None;
    if let Some(first) = right_items.first() {
        let pane = spawn_wezterm_pane(
            project_dir,
            instance,
            wezterm_bin,
            anchor_pane,
            "--right",
            first,
            50,
        )?;
        spawned_panes.push(pane.clone());
        provider_panes.insert(first.clone(), pane.clone());
        right_remainder = Some(pane);
    }

    let right_total = right_items.len();
    for (i, provider) in right_items.iter().enumerate().skip(1) {
        let parent = right_remainder.as_deref().unwrap_or(anchor_pane);
        let percent = split_percent_for_equal_stack(right_total, i);
        let pane = spawn_wezterm_pane(
            project_dir,
            instance,
            wezterm_bin,
            parent,
            "--bottom",
            provider,
            percent,
        )?;
        spawned_panes.push(pane.clone());
        provider_panes.insert(provider.clone(), pane.clone());
        right_remainder = Some(pane);
    }

    let mut left_parent = anchor_pane.to_string();
    for provider in left_items {
        let pane = spawn_wezterm_pane(
            project_dir,
            instance,
            wezterm_bin,
            &left_parent,
            "--bottom",
            provider,
            50,
        )?;
        spawned_panes.push(pane.clone());
        provider_panes.insert(provider.clone(), pane.clone());
        left_parent = pane;
    }

    Ok(())
}

fn spawn_wezterm_pane(
    project_dir: &Path,
    instance: &str,
    wezterm_bin: &str,
    parent: &str,
    direction_flag: &str,
    provider: &str,
    percent: u8,
) -> Result<String> {
    let provider_cmd = provider_start_cmd(project_dir, instance, provider);
    let pane_id =
        spawn_wezterm_custom_pane(wezterm_bin, parent, direction_flag, percent, &provider_cmd)?;
    println!(
        "已拉起 provider CLI：provider={} backend=wezterm pane={}",
        provider, pane_id
    );
    Ok(pane_id)
}

fn split_percent_for_equal_stack(total_items: usize, next_index: usize) -> u8 {
    // Split current remainder pane so all panes in the stack converge to equal height.
    // `next_index` is the index (>=1) of the provider being created in that stack.
    let m = total_items.saturating_sub(next_index).saturating_add(1);
    if m <= 1 {
        return 50;
    }
    let pct = ((m - 1) * 100 + m / 2) / m;
    pct.clamp(10, 90) as u8
}

fn spawn_tmux_custom_pane(
    parent: &str,
    direction: &str,
    percent: Option<u8>,
    command: &str,
    title: &str,
) -> Result<String> {
    let full_cmd = wrap_shell_command(command);
    let (flag, before) = match direction {
        "right" => ("-h", false),
        "bottom" => ("-v", false),
        "top" => ("-v", true),
        other => bail!("不支持的 tmux 分屏方向：{}", other),
    };

    let mut args: Vec<String> = vec![
        "split-window".to_string(),
        "-P".to_string(),
        "-F".to_string(),
        "#{pane_id}".to_string(),
        "-t".to_string(),
        parent.to_string(),
    ];
    if before {
        args.push("-b".to_string());
    }
    args.push(flag.to_string());
    if let Some(p) = percent {
        args.push("-p".to_string());
        args.push(p.to_string());
    }
    args.push(full_cmd);

    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let pane_id = run_capture("tmux", &arg_refs, "tmux split-window 失败")?;
    let pane_id = pane_id.trim().to_string();
    if pane_id.is_empty() {
        bail!("tmux split-window 未返回 pane id");
    }

    let _ = run_simple(
        "tmux",
        &["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"],
    );
    if !title.trim().is_empty() {
        let _ = run_simple("tmux", &["select-pane", "-t", &pane_id, "-T", title]);
    }
    Ok(pane_id)
}

fn spawn_wezterm_custom_pane(
    wezterm_bin: &str,
    parent: &str,
    direction_flag: &str,
    percent: u8,
    command: &str,
) -> Result<String> {
    let shell = resolve_shell_path();
    let args = vec![
        "cli".to_string(),
        "split-pane".to_string(),
        "--pane-id".to_string(),
        parent.to_string(),
        direction_flag.to_string(),
        "--percent".to_string(),
        percent.to_string(),
        "--".to_string(),
        shell,
        "-lc".to_string(),
        command.to_string(),
    ];
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let pane_id = run_capture(
        wezterm_bin,
        &arg_refs,
        "wezterm split-pane 失败（请检查 WEZTERM_PANE 和 wezterm cli 可用性）",
    )?;
    let pane_id = pane_id.trim().to_string();
    if pane_id.is_empty() {
        bail!("wezterm split-pane 未返回 pane id");
    }
    Ok(pane_id)
}

fn maybe_spawn_debug_watch_pane(
    project_dir: &Path,
    instance: &str,
    providers: &[String],
    backend: &LaunchBackend,
    orchestrator_pane: &str,
) -> Result<Option<String>> {
    if !debug_watch_pane_enabled() {
        return Ok(None);
    }
    if providers.is_empty() {
        return Ok(None);
    }

    let watch_provider = resolve_debug_watch_provider(providers);
    let watch_cmd = build_debug_watch_command(project_dir, instance, watch_provider.as_deref())?;
    let pane_percent = debug_watch_pane_percent();
    let pane_title = watch_provider
        .as_ref()
        .map(|provider| format!("RCCB-LOG-{}", provider))
        .unwrap_or_else(|| "RCCB-LOG-ALL".to_string());
    let pane = match backend {
        LaunchBackend::Tmux { .. } => spawn_tmux_custom_pane(
            orchestrator_pane,
            "top",
            Some(pane_percent),
            &watch_cmd,
            &pane_title,
        )?,
        LaunchBackend::Wezterm { bin, .. } => {
            spawn_wezterm_custom_pane(bin, orchestrator_pane, "--top", pane_percent, &watch_cmd)?
        }
    };
    println!(
        "已拉起 debug 日志 pane：scope={} pane={}",
        watch_provider.unwrap_or_else(|| "all".to_string()),
        pane
    );
    Ok(Some(pane))
}

fn build_debug_watch_command(
    project_dir: &Path,
    instance: &str,
    provider: Option<&str>,
) -> Result<String> {
    let exe = env::current_exe().context("获取当前 rccb 可执行文件路径失败")?;
    let scope = match provider {
        Some(provider) => format!("--provider {}", shell_quote(provider)),
        None => "--all".to_string(),
    };
    Ok(format!(
        "{exe} --project-dir {project} watch --instance {instance} {scope} --with-provider-log --with-debug-log --follow --timeout-s 0 --pane-ui",
        exe = shell_quote(&exe.display().to_string()),
        project = shell_quote(&project_dir.display().to_string()),
        instance = shell_quote(instance),
        scope = scope,
    ))
}

fn resolve_debug_watch_provider(providers: &[String]) -> Option<String> {
    let Ok(raw) = env::var("RCCB_DEBUG_WATCH_PROVIDER") else {
        return None;
    };
    let candidate = raw.trim();
    if candidate.is_empty() {
        return None;
    }
    if matches!(candidate.to_ascii_lowercase().as_str(), "all" | "*") {
        return None;
    }

    let normalized = match normalize_provider(candidate) {
        Ok(v) => v.to_string(),
        Err(err) => {
            eprintln!(
                "警告：RCCB_DEBUG_WATCH_PROVIDER 无效（{}），回退为全局 debug 视图",
                err
            );
            return None;
        }
    };
    if providers.iter().any(|p| p == &normalized) {
        Some(normalized)
    } else {
        eprintln!(
            "警告：RCCB_DEBUG_WATCH_PROVIDER={} 不在当前 provider 列表中，回退为全局 debug 视图",
            normalized
        );
        None
    }
}

fn debug_watch_pane_enabled() -> bool {
    env_bool("RCCB_DEBUG_WATCH_PANE", true)
}

fn debug_watch_pane_percent() -> u8 {
    let default = 25u8;
    let Ok(raw) = env::var("RCCB_DEBUG_WATCH_PANE_PERCENT") else {
        return default;
    };
    let value = match raw.trim().parse::<u16>() {
        Ok(v) => v,
        Err(_) => return default,
    };
    value.clamp(10, 80) as u8
}

fn maybe_prime_orchestrator_pane(
    backend: &LaunchBackend,
    pane_id: &str,
    orchestrator: &str,
    executors: &[String],
) {
    if !orchestrator_strict_mode_enabled(executors) {
        return;
    }
    let Some(target) = pane_dispatch_target_from_launch_backend(backend, pane_id) else {
        return;
    };
    let prompt = orchestrator_guardrail_prompt(orchestrator, executors);
    let delay_ms = orchestrator_prime_delay_ms();
    thread::spawn(move || {
        if delay_ms > 0 {
            thread::sleep(Duration::from_millis(delay_ms));
        }
        if let Err(err) = dispatch_text_to_pane(&target, &prompt) {
            eprintln!("警告：编排者 strict guardrail 注入失败：{}", err);
        }
    });
}

fn pane_dispatch_target_from_launch_backend(
    backend: &LaunchBackend,
    pane_id: &str,
) -> Option<PaneDispatchTarget> {
    let pane = pane_id.trim();
    if pane.is_empty() {
        return None;
    }
    let backend = match backend {
        LaunchBackend::Tmux { .. } => ProviderPaneBackend::Tmux,
        LaunchBackend::Wezterm { bin, .. } => ProviderPaneBackend::Wezterm { bin: bin.clone() },
    };
    Some(PaneDispatchTarget {
        backend,
        pane_id: pane.to_string(),
    })
}

fn orchestrator_strict_mode_enabled(executors: &[String]) -> bool {
    !executors.is_empty() && env_bool("RCCB_ORCHESTRATOR_STRICT", true)
}

fn orchestrator_prime_delay_ms() -> u64 {
    match env::var("RCCB_ORCHESTRATOR_PRIME_DELAY_MS") {
        Ok(raw) => raw.trim().parse::<u64>().unwrap_or(1200).min(10000),
        Err(_) => 1200,
    }
}

fn orchestrator_guardrail_prompt(orchestrator: &str, executors: &[String]) -> String {
    let executor_list = if executors.is_empty() {
        "-".to_string()
    } else {
        executors.join(", ")
    };
    format!(
        "RCCB 编排模式已启用。\n\n你当前是编排者：{orchestrator}。\n可用执行者：{executor_list}。\n\n严格规则：\n- 不要自己执行 bash 命令。\n- 不要自己修改文件或运行测试。\n- 所有执行任务都必须通过 RCCB 委派给执行者。\n- 你的职责只包括：规划、拆解、分派、验收、汇总。\n\n推荐委派格式：\n`rccb --project-dir . ask --instance default --provider <执行者> --caller {orchestrator} \"<任务>\"`\n\n运行期间，执行者状态会先写入后台 inbox；如启用状态回调，系统会间歇性推送有效进展给你。\n最终结果会通过 RCCB callback 回注；如果回注失败，系统会自动重试。\n\n收到 `RCCB_RESULT` 后再继续编排；如果还需要动作，请再次委派，而不是自己执行。"
    )
}

fn run_orchestrator_foreground(project_dir: &Path, instance: &str, provider: &str) -> Result<i32> {
    let cmd = provider_start_cmd(project_dir, instance, provider);
    println!("编排者进入前台：provider={}", provider);
    let status = ProcessCommand::new(resolve_shell_path())
        .arg("-lc")
        .arg(&cmd)
        .status()
        .with_context(|| format!("启动编排者命令失败：{}", cmd))?;
    Ok(status.code().unwrap_or(1))
}

fn cleanup_after_orchestrator(
    project_dir: &Path,
    backend: &LaunchBackend,
    spawned_panes: &[String],
) -> Result<()> {
    for pane in spawned_panes.iter().rev() {
        match backend {
            LaunchBackend::Tmux { .. } => {
                let _ = run_simple("tmux", &["kill-pane", "-t", pane]);
            }
            LaunchBackend::Wezterm { bin, .. } => {
                let _ = run_simple(bin, &["cli", "kill-pane", "--pane-id", pane]);
            }
        }
    }

    let _ = cmd_stop(project_dir, SHORTCUT_INSTANCE);
    if let Ok(cleaned) = cleanup_inflight_tasks(project_dir, SHORTCUT_INSTANCE) {
        if cleaned > 0 {
            println!("编排者退出清理：已终止 {} 个未完成任务", cleaned);
        }
    }
    let _ = fs::remove_dir_all(tmp_instance_dir(project_dir, SHORTCUT_INSTANCE).join("launcher"));
    Ok(())
}

fn cleanup_inflight_tasks(project_dir: &Path, instance: &str) -> Result<usize> {
    let dir = tasks_instance_dir(project_dir, instance);
    if !dir.exists() {
        return Ok(0);
    }

    let done_at = now_unix();
    let mut cleaned = 0usize;
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        if !path.is_file() || path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }

        let raw = match fs::read_to_string(&path) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let val: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let status = val
            .get("status")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if status != "queued" && status != "running" {
            continue;
        }

        let _ = update_task_status(
            &path,
            "canceled",
            None,
            Some(done_at),
            Some(130),
            Some("orchestrator exited; task canceled during cleanup"),
        );
        cleaned += 1;
    }
    Ok(cleaned)
}

fn prepare_launcher_runtime(
    project_dir: &Path,
    instance: &str,
    providers: &[String],
    backend: &LaunchBackend,
    provider_panes: &HashMap<String, String>,
) -> Result<()> {
    let launcher_meta = launcher_meta_path(project_dir, instance);
    if let Some(parent) = launcher_meta.parent() {
        fs::create_dir_all(parent)?;
    }

    let orchestrator = providers
        .first()
        .cloned()
        .unwrap_or_else(|| "orchestrator".to_string());
    let mut entries = Vec::new();
    for provider in providers {
        entries.push(LauncherProviderMeta {
            provider: provider.clone(),
            role: if provider == &orchestrator {
                "orchestrator".to_string()
            } else {
                "executor".to_string()
            },
            feed_file: String::new(),
            pane_id: provider_panes.get(provider).cloned(),
            pane_title: Some(format!("RCCB-{}", provider)),
        });
    }

    let (backend_name, backend_bin) = match backend {
        LaunchBackend::Tmux { .. } => ("tmux".to_string(), None),
        LaunchBackend::Wezterm { bin, .. } => ("wezterm".to_string(), Some(bin.clone())),
    };
    let meta = LauncherMeta {
        schema_version: 1,
        instance: instance.to_string(),
        created_at_unix: now_unix(),
        backend: backend_name,
        backend_bin,
        orchestrator,
        providers: entries,
    };
    write_json_pretty(&launcher_meta, &meta)
}

fn provider_start_cmd(_project_dir: &Path, _instance: &str, provider: &str) -> String {
    provider_raw_start_cmd(provider)
}

fn provider_raw_start_cmd(provider: &str) -> String {
    let key = format!("RCCB_{}_START_CMD", provider.trim().to_ascii_uppercase());
    if let Ok(v) = env::var(&key) {
        let v = v.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }

    if env_bool("RCCB_USE_BRIDGE_PROVIDER_LAUNCH", false) {
        if let Some(ccb_cmd) = provider_bridge_start_cmd(provider) {
            return ccb_cmd;
        }
    }

    match provider.trim().to_ascii_lowercase().as_str() {
        "claude" => "claude".to_string(),
        "codex" => "codex".to_string(),
        "gemini" => "gemini".to_string(),
        "opencode" => "opencode".to_string(),
        "droid" => "droid".to_string(),
        other => other.to_string(),
    }
}

fn provider_bridge_start_cmd(provider: &str) -> Option<String> {
    let launch = resolve_bridge_launch_cmd()?;
    let provider = provider.trim().to_ascii_lowercase();
    if provider.is_empty() {
        return None;
    }
    Some(format!(
        "{} {} {}",
        rccb_autostart_exports(),
        launch,
        shell_quote(&provider)
    ))
}

fn resolve_bridge_launch_cmd() -> Option<String> {
    if let Ok(v) = env::var("RCCB_BRIDGE_PROVIDER_LAUNCH_CMD") {
        let v = v.trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

fn rccb_autostart_exports() -> String {
    [
        "RCCB_ASKD_AUTOSTART=1",
        "RCCB_CASKD_AUTOSTART=1",
        "RCCB_GASKD_AUTOSTART=1",
        "RCCB_OASKD_AUTOSTART=1",
        "RCCB_LASKD_AUTOSTART=1",
        "RCCB_DASKD_AUTOSTART=1",
        "RCCB_CASKD=1",
        "RCCB_GASKD=1",
        "RCCB_OASKD=1",
        "RCCB_LASKD=1",
        "RCCB_DASKD=1",
        "RCCB_AUTO_CASKD=1",
        "RCCB_AUTO_GASKD=1",
        "RCCB_AUTO_OASKD=1",
        "RCCB_AUTO_LASKD=1",
        "RCCB_AUTO_DASKD=1",
    ]
    .join(" ")
}

fn env_bool(key: &str, default: bool) -> bool {
    let Ok(raw) = env::var(key) else {
        return default;
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => default,
    }
}

fn resolve_shell_path() -> String {
    env::var("SHELL")
        .map(|v| v.trim().to_string())
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/bin/bash".to_string())
}

fn wrap_shell_command(cmd: &str) -> String {
    format!("{} -lc {}", resolve_shell_path(), shell_quote(cmd))
}

fn shell_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\"'\"'"))
}

fn run_capture(bin: &str, args: &[&str], err_ctx: &str) -> Result<String> {
    let out = ProcessCommand::new(bin)
        .args(args)
        .output()
        .with_context(|| format!("{}: {} {:?}", err_ctx, bin, args))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        bail!(
            "{}: status={} stdout=`{}` stderr=`{}`",
            err_ctx,
            out.status,
            stdout,
            stderr
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn run_simple(bin: &str, args: &[&str]) -> Result<()> {
    let status = ProcessCommand::new(bin).args(args).status()?;
    if status.success() {
        return Ok(());
    }
    bail!("command failed: {} {:?} status={}", bin, args, status);
}

pub fn cmd_status(project_dir: &Path, instance: Option<&str>, as_json: bool) -> Result<()> {
    ensure_project_layout(project_dir)?;

    let items = if let Some(name) = instance {
        let path = state_path(project_dir, name);
        if !path.exists() {
            vec![]
        } else {
            vec![load_state(&path)?]
        }
    } else {
        load_all_states(project_dir)?
    };

    let mut output = Vec::new();
    for mut s in items {
        if s.status == "running" && !is_process_alive(s.pid) {
            s.status = "stale".to_string();
        }
        output.push(s);
    }
    let in_flight_map = collect_in_flight_map(project_dir, &output)?;

    if as_json {
        let mut instances = Vec::new();
        for s in output {
            let mut val = serde_json::to_value(&s)?;
            if let Some(obj) = val.as_object_mut() {
                let in_flight = in_flight_map
                    .get(&s.instance_id)
                    .cloned()
                    .unwrap_or_default();
                obj.insert("in_flight_count".to_string(), json!(in_flight.len()));
                obj.insert("in_flight_req_ids".to_string(), json!(in_flight));
            }
            instances.push(val);
        }
        let val = json!({
            "project": project_dir.display().to_string(),
            "instances": instances,
        });
        println!("{}", serde_json::to_string_pretty(&val)?);
        return Ok(());
    }

    if output.is_empty() {
        println!("未找到实例：project={}", project_dir.display());
        return Ok(());
    }

    println!("项目：{}", project_dir.display());
    for s in output {
        println!(
            "- instance={} pid={} status={} started={} last_heartbeat={} stopped={}",
            s.instance_id,
            s.pid,
            s.status,
            s.started_at_unix,
            s.last_heartbeat_unix,
            s.stopped_at_unix
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string())
        );
        println!(
            "  debug={} debug_log={}",
            if s.debug_enabled { "开" } else { "关" },
            logs_instance_dir(project_dir, &s.instance_id)
                .join("debug.log")
                .display()
        );

        if !s.providers.is_empty() {
            println!("  providers={}", s.providers.join(","));
            println!(
                "  orchestrator={} executors={}",
                s.orchestrator.unwrap_or_else(|| "-".to_string()),
                if s.executors.is_empty() {
                    "-".to_string()
                } else {
                    s.executors.join(",")
                }
            );
            println!(
                "  daemon={}:{} session_file={} last_task_id={}",
                s.daemon_host.unwrap_or_else(|| "-".to_string()),
                s.daemon_port
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                s.session_file.unwrap_or_else(|| "-".to_string()),
                s.last_task_id.unwrap_or_else(|| "-".to_string())
            );
            let in_flight = in_flight_map
                .get(&s.instance_id)
                .cloned()
                .unwrap_or_default();
            println!(
                "  in_flight={} req_ids={}",
                in_flight.len(),
                if in_flight.is_empty() {
                    "-".to_string()
                } else {
                    in_flight.join(",")
                }
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
struct MountedProviderView {
    provider: String,
    role: String,
    session_file: String,
    session_exists: bool,
    mounted: bool,
}

#[derive(Debug, Clone, Serialize)]
struct MountedInstanceView {
    instance: String,
    status: String,
    daemon_online: bool,
    providers: Vec<MountedProviderView>,
}

pub fn cmd_mounted(project_dir: &Path, instance: Option<&str>, as_json: bool) -> Result<()> {
    ensure_project_layout(project_dir)?;

    let items = if let Some(name) = instance {
        let path = state_path(project_dir, name);
        if !path.exists() {
            vec![]
        } else {
            vec![load_state(&path)?]
        }
    } else {
        load_all_states(project_dir)?
    };

    let mut mounted_views = Vec::new();
    for mut s in items {
        if s.status == "running" && !is_process_alive(s.pid) {
            s.status = "stale".to_string();
        }
        let daemon_online = s.status == "running" && ping_daemon_state(&s, 0.8).is_ok();

        let providers_dir = session_instance_dir(project_dir, &s.instance_id).join("providers");
        let providers = s
            .providers
            .iter()
            .map(|provider| {
                let session_file = providers_dir.join(format!("{}.json", provider));
                let session_exists = session_file.exists();
                let role = if s.orchestrator.as_deref() == Some(provider.as_str()) {
                    "orchestrator"
                } else {
                    "executor"
                };
                MountedProviderView {
                    provider: provider.clone(),
                    role: role.to_string(),
                    session_file: session_file.display().to_string(),
                    session_exists,
                    mounted: daemon_online && session_exists,
                }
            })
            .collect::<Vec<_>>();

        mounted_views.push(MountedInstanceView {
            instance: s.instance_id,
            status: s.status,
            daemon_online,
            providers,
        });
    }

    if as_json {
        let val = json!({
            "project": project_dir.display().to_string(),
            "instances": mounted_views,
        });
        println!("{}", serde_json::to_string_pretty(&val)?);
        return Ok(());
    }

    if mounted_views.is_empty() {
        println!("未找到实例：project={}", project_dir.display());
        return Ok(());
    }

    println!("项目：{}", project_dir.display());
    for item in mounted_views {
        println!(
            "- instance={} status={} daemon_online={}",
            item.instance,
            item.status,
            if item.daemon_online { "是" } else { "否" }
        );
        if item.providers.is_empty() {
            println!("  providers=-");
            continue;
        }
        for p in item.providers {
            println!(
                "  - provider={} role={} mounted={} session_exists={} session_file={}",
                p.provider,
                p.role,
                if p.mounted { "是" } else { "否" },
                if p.session_exists { "是" } else { "否" },
                p.session_file
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct TaskView {
    instance: String,
    task_id: String,
    req_id: Option<String>,
    provider: Option<String>,
    status: String,
    created_at_unix: Option<u64>,
    started_at_unix: Option<u64>,
    completed_at_unix: Option<u64>,
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply: Option<String>,
}

pub fn cmd_tasks(
    project_dir: &Path,
    instance: Option<&str>,
    limit: usize,
    as_json: bool,
) -> Result<()> {
    ensure_project_layout(project_dir)?;
    let mut items = collect_tasks(project_dir, instance)?;
    items.sort_by(|a, b| {
        b.created_at_unix
            .unwrap_or(0)
            .cmp(&a.created_at_unix.unwrap_or(0))
    });

    if limit > 0 && items.len() > limit {
        items.truncate(limit);
    }

    if as_json {
        let val = json!({
            "project": project_dir.display().to_string(),
            "tasks": items,
        });
        println!("{}", serde_json::to_string_pretty(&val)?);
        return Ok(());
    }

    if items.is_empty() {
        println!("未找到任务：project={}", project_dir.display());
        return Ok(());
    }

    println!("项目={} 任务数={}", project_dir.display(), items.len());
    for t in items {
        println!(
            "- instance={} task_id={} req_id={} provider={} status={} exit={} created={} started={} completed={}",
            t.instance,
            t.task_id,
            t.req_id.unwrap_or_else(|| "-".to_string()),
            t.provider.unwrap_or_else(|| "-".to_string()),
            t.status,
            t.exit_code
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string()),
            t.created_at_unix
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string()),
            t.started_at_unix
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string()),
            t.completed_at_unix
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string()),
        );
    }

    Ok(())
}

fn collect_tasks(project_dir: &Path, instance: Option<&str>) -> Result<Vec<TaskView>> {
    if let Some(name) = instance {
        return load_tasks_in_instance(project_dir, name);
    }

    let mut out = Vec::new();
    for entry in fs::read_dir(tasks_root_dir(project_dir))? {
        let e = entry?;
        let path = e.path();
        if !path.is_dir() {
            continue;
        }
        let instance_name = e.file_name().to_string_lossy().to_string();
        out.extend(load_tasks_from_dir(&path, &instance_name)?);
    }
    Ok(out)
}

fn load_tasks_in_instance(project_dir: &Path, instance: &str) -> Result<Vec<TaskView>> {
    let dir = tasks_instance_dir(project_dir, instance);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    load_tasks_from_dir(&dir, instance)
}

fn load_tasks_from_dir(dir: &Path, instance: &str) -> Result<Vec<TaskView>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let e = entry?;
        let path = e.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }

        let raw = match fs::read_to_string(&path) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let v: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let task_id = v
            .get("task_id")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|x| x.to_str())
                    .unwrap_or("unknown")
                    .to_string()
            });

        out.push(TaskView {
            instance: instance.to_string(),
            task_id,
            req_id: v
                .get("req_id")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
            provider: v
                .get("provider")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
            status: v
                .get("status")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown")
                .to_string(),
            created_at_unix: v.get("created_at_unix").and_then(|x| x.as_u64()),
            started_at_unix: v.get("started_at_unix").and_then(|x| x.as_u64()),
            completed_at_unix: v.get("completed_at_unix").and_then(|x| x.as_u64()),
            exit_code: v
                .get("exit_code")
                .and_then(|x| x.as_i64())
                .map(|x| x as i32),
            reply: v
                .get("reply")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
        });
    }
    Ok(out)
}

fn collect_in_flight_map(
    project_dir: &Path,
    states: &[InstanceState],
) -> Result<HashMap<String, Vec<String>>> {
    let mut out = HashMap::new();
    for state in states {
        if state.status != "running" {
            out.insert(state.instance_id.clone(), Vec::new());
            continue;
        }
        out.insert(
            state.instance_id.clone(),
            collect_in_flight_req_ids(project_dir, &state.instance_id)?,
        );
    }
    Ok(out)
}

fn collect_in_flight_req_ids(project_dir: &Path, instance: &str) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for task in load_tasks_in_instance(project_dir, instance)? {
        if !is_in_flight_status(&task.status) {
            continue;
        }
        if let Some(req_id) = task.req_id {
            if !ids.iter().any(|x| x == &req_id) {
                ids.push(req_id);
            }
        }
    }
    ids.sort();
    Ok(ids)
}

fn is_in_flight_status(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "queued" | "running"
    )
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_watch(
    project_dir: &Path,
    instance: &str,
    req_id: Option<&str>,
    provider: Option<&str>,
    all: bool,
    poll_ms: u64,
    timeout_s: f64,
    follow: bool,
    with_provider_log: bool,
    with_debug_log: bool,
    pane_ui: bool,
    as_json: bool,
) -> Result<()> {
    ensure_project_layout(project_dir)?;
    let fixed_req_id = req_id
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let watch_provider = provider
        .map(normalize_provider)
        .transpose()?
        .map(|v| v.to_string());

    if all && (fixed_req_id.is_some() || watch_provider.is_some()) {
        bail!("--all 不能与 --req-id 或 --provider 同时使用");
    }
    if fixed_req_id.is_some() && watch_provider.is_some() {
        bail!("--req-id 与 --provider 不能同时使用");
    }
    if !all && fixed_req_id.is_none() && watch_provider.is_none() {
        bail!("需要提供 --req-id 或 --provider");
    }

    let effective_timeout_s =
        if follow && fixed_req_id.is_none() && (watch_provider.is_some() || all) {
            -1.0
        } else {
            timeout_s
        };

    if watch_bus_enabled() {
        if all {
            return watch_all_via_bus(
                project_dir,
                instance,
                effective_timeout_s,
                with_provider_log,
                with_debug_log,
                pane_ui,
                as_json,
            );
        }
        match watch_via_bus(
            project_dir,
            instance,
            fixed_req_id.as_deref(),
            watch_provider.as_deref(),
            effective_timeout_s,
            follow,
            with_provider_log,
            with_debug_log,
            pane_ui,
            as_json,
        ) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(err) => {
                if !as_json {
                    eprintln!("watch: 实时总线不可用，回退轮询模式。原因：{}", err);
                }
            }
        }
    }

    let poll = Duration::from_millis(poll_ms.max(50));
    let deadline = if effective_timeout_s <= 0.0 {
        None
    } else {
        Some(Instant::now() + Duration::from_secs_f64(effective_timeout_s.max(0.1)))
    };

    let mut last_task: Option<TaskView> = None;
    let mut provider_log_offset = 0u64;
    let mut debug_log_offset = 0u64;
    let mut printed_waiting = false;
    let mut current_req_id = fixed_req_id.clone();
    let mut announced_req_id: Option<String> = None;
    let follow_started_at = now_unix();
    let mut followed_done_req_ids: HashSet<String> = HashSet::new();
    let tail_like_quiet = follow && with_provider_log && !as_json;
    let pane_mode = pane_ui && !as_json;

    if pane_mode {
        emit_watch_pane_header(instance, watch_provider.as_deref(), fixed_req_id.as_deref());
    }

    loop {
        if let Some(limit) = deadline {
            if Instant::now() >= limit {
                bail!(
                    "watch timeout: instance={} req_id={} provider={} timeout_s={}",
                    instance,
                    current_req_id.unwrap_or_else(|| "-".to_string()),
                    watch_provider.clone().unwrap_or_else(|| "-".to_string()),
                    effective_timeout_s
                );
            }
        }

        if fixed_req_id.is_none() {
            if let Some(provider_name) = watch_provider.as_deref() {
                let next_req_id = if follow {
                    select_watch_req_for_provider_follow(
                        project_dir,
                        instance,
                        provider_name,
                        &followed_done_req_ids,
                        follow_started_at,
                    )?
                } else {
                    select_watch_req_for_provider(project_dir, instance, provider_name)?
                };
                if next_req_id != current_req_id {
                    current_req_id = next_req_id;
                    announced_req_id = None;
                    last_task = None;
                    provider_log_offset = 0;
                    debug_log_offset = 0;
                    printed_waiting = false;
                }
            }
        }

        let Some(active_req_id) = current_req_id.as_deref() else {
            if !printed_waiting && !tail_like_quiet && !pane_mode {
                if as_json {
                    println!(
                        "{}",
                        serde_json::to_string(&json!({
                            "event": "waiting",
                            "instance": instance,
                            "provider": watch_provider.clone().unwrap_or_default(),
                        }))?
                    );
                } else {
                    println!(
                        "watch waiting: instance={} provider={} req_id=-",
                        instance,
                        watch_provider.clone().unwrap_or_else(|| "-".to_string())
                    );
                }
                printed_waiting = true;
            }
            thread::sleep(poll);
            continue;
        };

        if announced_req_id.as_deref() != Some(active_req_id) {
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "event": "track",
                        "instance": instance,
                        "provider": watch_provider.clone().unwrap_or_default(),
                        "req_id": active_req_id,
                    }))?
                );
            } else if let Some(provider_name) = watch_provider.as_deref() {
                if tail_like_quiet {
                    announced_req_id = Some(active_req_id.to_string());
                    continue;
                }
                if pane_mode {
                    println!(
                        "[track] provider={} req_id={}",
                        provider_name, active_req_id
                    );
                } else {
                    println!(
                        "watch tracking: instance={} provider={} req_id={}",
                        instance, provider_name, active_req_id
                    );
                }
            }
            announced_req_id = Some(active_req_id.to_string());
        }

        let task = load_task_by_req_id(project_dir, instance, active_req_id)?;
        match task {
            Some(cur) => {
                if last_task.as_ref() != Some(&cur) && !tail_like_quiet {
                    emit_watch_task(&cur, as_json, pane_mode)?;
                    last_task = Some(cur.clone());
                }

                if with_provider_log {
                    if let Some(provider) = cur.provider.as_deref() {
                        let provider_log = logs_instance_dir(project_dir, instance)
                            .join(format!("{}.log", provider));
                        tail_log_for_req(
                            &provider_log,
                            active_req_id,
                            "provider",
                            &mut provider_log_offset,
                            pane_mode,
                            as_json,
                        )?;
                    }
                }

                if with_debug_log {
                    let debug_log = logs_instance_dir(project_dir, instance).join("debug.log");
                    tail_log_for_req(
                        &debug_log,
                        active_req_id,
                        "debug",
                        &mut debug_log_offset,
                        pane_mode,
                        as_json,
                    )?;
                }

                if is_terminal_task_status(&cur.status) {
                    if follow && fixed_req_id.is_none() && watch_provider.is_some() {
                        if let Some(done_req_id) = cur.req_id.clone() {
                            followed_done_req_ids.insert(done_req_id);
                        }
                        current_req_id = None;
                        announced_req_id = None;
                        last_task = None;
                        provider_log_offset = 0;
                        debug_log_offset = 0;
                        printed_waiting = false;
                        continue;
                    }
                    return Ok(());
                }
            }
            None => {
                if !printed_waiting && !tail_like_quiet && !pane_mode {
                    if as_json {
                        println!(
                            "{}",
                            serde_json::to_string(&json!({
                                "event": "waiting",
                                "instance": instance,
                                "req_id": active_req_id
                            }))?
                        );
                    } else {
                        println!(
                            "watch waiting: instance={} req_id={}",
                            instance, active_req_id
                        );
                    }
                    printed_waiting = true;
                }
            }
        }

        thread::sleep(poll);
    }
}

fn watch_all_via_bus(
    project_dir: &Path,
    instance: &str,
    effective_timeout_s: f64,
    with_provider_log: bool,
    with_debug_log: bool,
    pane_ui: bool,
    as_json: bool,
) -> Result<()> {
    let state_file = state_path(project_dir, instance);
    if !state_file.exists() {
        bail!("watch all failed: instance={} not started", instance);
    }
    let state = load_state(&state_file)?;
    if state.status != "running" || !is_process_alive(state.pid) {
        bail!("watch all failed: instance={} daemon not running", instance);
    }
    let host = state
        .daemon_host
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow!("watch all failed: missing daemon_host"))?;
    let port = state
        .daemon_port
        .ok_or_else(|| anyhow!("watch all failed: missing daemon_port"))?;
    let token = state
        .daemon_token
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow!("watch all failed: missing daemon_token"))?;

    run_watch_all_via_bus(
        project_dir,
        instance,
        &host,
        port,
        &token,
        effective_timeout_s,
        with_provider_log,
        with_debug_log,
        pane_ui,
        as_json,
    )
}

fn watch_bus_enabled() -> bool {
    let raw = std::env::var("RCCB_WATCH_BUS").unwrap_or_else(|_| "1".to_string());
    !matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no"
    )
}

#[allow(clippy::too_many_arguments)]
fn watch_via_bus(
    project_dir: &Path,
    instance: &str,
    fixed_req_id: Option<&str>,
    watch_provider: Option<&str>,
    effective_timeout_s: f64,
    follow: bool,
    with_provider_log: bool,
    with_debug_log: bool,
    pane_ui: bool,
    as_json: bool,
) -> Result<bool> {
    let state_file = state_path(project_dir, instance);
    if !state_file.exists() {
        return Ok(false);
    }

    let state = load_state(&state_file)?;
    if state.status != "running" || !is_process_alive(state.pid) {
        return Ok(false);
    }

    let host = match state.daemon_host {
        Some(v) if !v.trim().is_empty() => v,
        _ => return Ok(false),
    };
    let port = match state.daemon_port {
        Some(v) => v,
        None => return Ok(false),
    };
    let token = match state.daemon_token {
        Some(v) if !v.trim().is_empty() => v,
        _ => return Ok(false),
    };

    run_watch_via_bus(
        project_dir,
        instance,
        &host,
        port,
        &token,
        fixed_req_id,
        watch_provider,
        effective_timeout_s,
        follow,
        with_provider_log,
        with_debug_log,
        pane_ui,
        as_json,
    )?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn run_watch_via_bus(
    project_dir: &Path,
    instance: &str,
    host: &str,
    port: u16,
    token: &str,
    fixed_req_id: Option<&str>,
    watch_provider: Option<&str>,
    effective_timeout_s: f64,
    follow: bool,
    with_provider_log: bool,
    with_debug_log: bool,
    pane_ui: bool,
    as_json: bool,
) -> Result<()> {
    let deadline = if effective_timeout_s <= 0.0 {
        None
    } else {
        Some(Instant::now() + Duration::from_secs_f64(effective_timeout_s.max(0.1)))
    };

    let follow_started_at = now_unix();
    let mut followed_done_req_ids: HashSet<String> = HashSet::new();
    let mut tracked_req_id = fixed_req_id.map(|v| v.to_string());
    if tracked_req_id.is_none() {
        if let Some(provider) = watch_provider {
            tracked_req_id = if follow {
                select_watch_req_for_provider_follow(
                    project_dir,
                    instance,
                    provider,
                    &followed_done_req_ids,
                    follow_started_at,
                )?
            } else {
                select_watch_req_for_provider(project_dir, instance, provider)?
            };
        }
    }

    let mut announced_req_id: Option<String> = None;
    let mut last_task: Option<TaskView> = None;
    let mut debug_log_offset = 0u64;
    let mut printed_waiting = false;
    let tail_like_quiet = follow && with_provider_log && !as_json;
    let pane_mode = pane_ui && !as_json;
    let mut last_seq = 0u64;
    let mut backoff = Duration::from_millis(120);
    let reconnect_cap = Duration::from_secs(2);

    if pane_mode {
        emit_watch_pane_header(instance, watch_provider, fixed_req_id);
    }

    if let Some(req_id) = tracked_req_id.as_deref() {
        if let Some(task) = load_task_by_req_id(project_dir, instance, req_id)? {
            if !tail_like_quiet {
                emit_watch_task(&task, as_json, pane_mode)?;
            }
            last_task = Some(task.clone());
            if is_terminal_task_status(&task.status) {
                if fixed_req_id.is_some() || !follow {
                    return Ok(());
                }
                followed_done_req_ids.insert(req_id.to_string());
                tracked_req_id = None;
                announced_req_id = None;
                last_task = None;
                debug_log_offset = 0;
            }
        }
    }

    loop {
        if let Some(limit) = deadline {
            if Instant::now() >= limit {
                bail!(
                    "watch timeout: instance={} req_id={} provider={} timeout_s={}",
                    instance,
                    tracked_req_id.clone().unwrap_or_else(|| "-".to_string()),
                    watch_provider.unwrap_or("-"),
                    effective_timeout_s
                );
            }
        }

        if tracked_req_id.is_none() && !tail_like_quiet && !printed_waiting && !pane_mode {
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "event": "waiting",
                        "instance": instance,
                        "provider": watch_provider.unwrap_or_default(),
                    }))?
                );
            } else {
                println!(
                    "watch waiting: instance={} provider={} req_id=-",
                    instance,
                    watch_provider.unwrap_or("-")
                );
            }
            printed_waiting = true;
        }

        let mut sub_req = json!({
            "type": format!("{}.subscribe", PROTOCOL_PREFIX),
            "v": PROTOCOL_VERSION,
            "id": format!("watch-{}-{}", std::process::id(), crate::io_utils::now_unix_ms()),
            "token": token,
            "follow": true,
            "from_now": false,
        });
        if let Some(provider) = watch_provider {
            sub_req["provider"] = Value::String(provider.to_string());
        }
        if fixed_req_id.is_some() {
            sub_req["req_id"] = Value::String(fixed_req_id.unwrap_or_default().to_string());
        }
        if last_seq > 0 {
            sub_req["from_seq"] = Value::Number(last_seq.into());
        }

        let mut reader = match connect_and_send(host, port, sub_req, 12.0) {
            Ok(v) => v,
            Err(err) => {
                if let Some(limit) = deadline {
                    if Instant::now() >= limit {
                        return Err(err.context("connect ask.subscribe failed (timeout reached)"));
                    }
                }
                thread::sleep(backoff);
                backoff = (backoff.saturating_mul(2)).min(reconnect_cap);
                continue;
            }
        };
        backoff = Duration::from_millis(120);

        loop {
            if let Some(limit) = deadline {
                if Instant::now() >= limit {
                    bail!(
                        "watch timeout: instance={} req_id={} provider={} timeout_s={}",
                        instance,
                        tracked_req_id.clone().unwrap_or_else(|| "-".to_string()),
                        watch_provider.unwrap_or("-"),
                        effective_timeout_s
                    );
                }
            }

            let mut line = String::new();
            let n = match reader.read_line(&mut line) {
                Ok(v) => v,
                Err(err) => {
                    if matches!(
                        err.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) {
                        continue;
                    }
                    break;
                }
            };
            if n == 0 {
                break;
            }

            let value: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let msg_type = value
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if msg_type == format!("{}.response", PROTOCOL_PREFIX) {
                let parsed: AskResponse =
                    serde_json::from_value(value).context("invalid ask.response in watch bus")?;
                if parsed.exit_code != 0 {
                    bail!(
                        "watch subscribe failed: exit_code={} reply={}",
                        parsed.exit_code,
                        parsed.reply
                    );
                }
                continue;
            }
            if msg_type != format!("{}.bus", PROTOCOL_PREFIX) {
                continue;
            }

            let event: AskBusEvent =
                serde_json::from_value(value).context("invalid ask.bus payload")?;
            if event.seq > last_seq {
                last_seq = event.seq;
            }

            if as_json {
                println!("{}", serde_json::to_string(&event)?);
            }

            if event.event == "keepalive" || event.event == "subscribed" {
                continue;
            }

            let event_req = event.req_id.as_deref();
            if let Some(fixed) = fixed_req_id {
                if event_req != Some(fixed) {
                    continue;
                }
            }

            if fixed_req_id.is_none() {
                if tracked_req_id.is_none() {
                    if let Some(rid) = event_req {
                        let old_terminal = event.ts_unix_ms / 1000 < follow_started_at
                            && is_terminal_bus_task_event(&event);
                        if !old_terminal || !follow {
                            tracked_req_id = Some(rid.to_string());
                            announced_req_id = None;
                            last_task = None;
                            debug_log_offset = 0;
                            printed_waiting = false;
                        }
                    }
                } else if !follow {
                    if let Some(rid) = event_req {
                        if tracked_req_id.as_deref() != Some(rid) {
                            continue;
                        }
                    }
                } else if let (Some(current), Some(rid)) = (tracked_req_id.as_deref(), event_req) {
                    if current != rid && !followed_done_req_ids.contains(current) {
                        continue;
                    }
                }
            }

            if !as_json {
                if let Some(active_req_id) = tracked_req_id.as_deref() {
                    if announced_req_id.as_deref() != Some(active_req_id) {
                        if let Some(provider_name) = watch_provider {
                            if !tail_like_quiet {
                                if pane_mode {
                                    println!(
                                        "[track] provider={} req_id={}",
                                        provider_name, active_req_id
                                    );
                                } else {
                                    println!(
                                        "watch tracking: instance={} provider={} req_id={}",
                                        instance, provider_name, active_req_id
                                    );
                                }
                            }
                        } else if !tail_like_quiet {
                            if pane_mode {
                                println!("[track] req_id={}", active_req_id);
                            } else {
                                println!(
                                    "watch tracking: instance={} req_id={}",
                                    instance, active_req_id
                                );
                            }
                        }
                        announced_req_id = Some(active_req_id.to_string());
                    }
                }
            }

            if with_provider_log && !as_json {
                if let Some(delta) = event.delta.as_deref() {
                    if !delta.trim().is_empty() {
                        emit_watch_bus_delta(
                            event.provider.as_deref().unwrap_or("provider"),
                            event.req_id.as_deref().unwrap_or("-"),
                            delta,
                            pane_mode,
                            false,
                        )?;
                    }
                }
            }

            if !as_json
                && !tail_like_quiet
                && matches!(event.event.as_str(), "dispatched" | "start" | "done")
            {
                if let Some(active_req_id) = tracked_req_id.as_deref() {
                    if let Some(cur) = load_task_by_req_id(project_dir, instance, active_req_id)? {
                        if last_task.as_ref() != Some(&cur) {
                            emit_watch_task(&cur, false, pane_mode)?;
                            last_task = Some(cur);
                        }
                    }
                }
            }

            if with_debug_log {
                if let Some(active_req_id) = tracked_req_id.as_deref() {
                    let debug_log = logs_instance_dir(project_dir, instance).join("debug.log");
                    tail_log_for_req(
                        &debug_log,
                        active_req_id,
                        "debug",
                        &mut debug_log_offset,
                        pane_mode,
                        as_json,
                    )?;
                }
            }

            if is_terminal_bus_task_event(&event) {
                let done_req_id = event.req_id.clone().unwrap_or_else(|| "-".to_string());
                if follow && fixed_req_id.is_none() && watch_provider.is_some() {
                    followed_done_req_ids.insert(done_req_id);
                    tracked_req_id = None;
                    announced_req_id = None;
                    last_task = None;
                    debug_log_offset = 0;
                    printed_waiting = false;
                    continue;
                }
                return Ok(());
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_watch_all_via_bus(
    project_dir: &Path,
    instance: &str,
    host: &str,
    port: u16,
    token: &str,
    effective_timeout_s: f64,
    with_provider_log: bool,
    with_debug_log: bool,
    pane_ui: bool,
    as_json: bool,
) -> Result<()> {
    let deadline = if effective_timeout_s <= 0.0 {
        None
    } else {
        Some(Instant::now() + Duration::from_secs_f64(effective_timeout_s.max(0.1)))
    };
    let mut debug_log_offset = 0u64;
    let mut last_seq = 0u64;
    let mut backoff = Duration::from_millis(120);
    let reconnect_cap = Duration::from_secs(2);

    if pane_ui {
        emit_watch_pane_header(instance, Some("all"), None);
    }

    loop {
        if let Some(limit) = deadline {
            if Instant::now() >= limit {
                bail!(
                    "watch timeout: instance={} scope=all timeout_s={}",
                    instance,
                    effective_timeout_s
                );
            }
        }

        let mut sub_req = json!({
            "type": format!("{}.subscribe", PROTOCOL_PREFIX),
            "v": PROTOCOL_VERSION,
            "id": format!("watch-all-{}-{}", std::process::id(), crate::io_utils::now_unix_ms()),
            "token": token,
            "follow": true,
            "from_now": false,
        });
        if last_seq > 0 {
            sub_req["from_seq"] = Value::Number(last_seq.into());
        }

        let mut reader = match connect_and_send(host, port, sub_req, 12.0) {
            Ok(v) => v,
            Err(err) => {
                if let Some(limit) = deadline {
                    if Instant::now() >= limit {
                        return Err(
                            err.context("connect ask.subscribe(all) failed (timeout reached)")
                        );
                    }
                }
                thread::sleep(backoff);
                backoff = (backoff.saturating_mul(2)).min(reconnect_cap);
                continue;
            }
        };
        backoff = Duration::from_millis(120);

        loop {
            if let Some(limit) = deadline {
                if Instant::now() >= limit {
                    bail!(
                        "watch timeout: instance={} scope=all timeout_s={}",
                        instance,
                        effective_timeout_s
                    );
                }
            }

            let mut line = String::new();
            let n = match reader.read_line(&mut line) {
                Ok(v) => v,
                Err(err) => {
                    if matches!(
                        err.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) {
                        continue;
                    }
                    break;
                }
            };
            if n == 0 {
                break;
            }

            let value: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let msg_type = value
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if msg_type == format!("{}.response", PROTOCOL_PREFIX) {
                let parsed: AskResponse =
                    serde_json::from_value(value).context("invalid ask.response in watch all")?;
                if parsed.exit_code != 0 {
                    bail!(
                        "watch subscribe(all) failed: exit_code={} reply={}",
                        parsed.exit_code,
                        parsed.reply
                    );
                }
                continue;
            }
            if msg_type != format!("{}.bus", PROTOCOL_PREFIX) {
                continue;
            }

            let event: AskBusEvent =
                serde_json::from_value(value).context("invalid ask.bus payload")?;
            if event.seq > last_seq {
                last_seq = event.seq;
            }

            if as_json {
                println!("{}", serde_json::to_string(&event)?);
                continue;
            }

            if event.event == "keepalive" || event.event == "subscribed" {
                continue;
            }

            if matches!(event.event.as_str(), "dispatched" | "start" | "done") {
                println!(
                    "[task] req_id={} provider={} status={} exit={}",
                    event.req_id.as_deref().unwrap_or("-"),
                    event.provider.as_deref().unwrap_or("-"),
                    event.status.as_deref().unwrap_or("-"),
                    event
                        .exit_code
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                );
                if event.event == "done" {
                    if let Some(reply) = event.reply.as_deref() {
                        if !reply.trim().is_empty() {
                            println!("[reply] {}", reply);
                        }
                    }
                }
            }

            if with_provider_log {
                if let Some(delta) = event.delta.as_deref() {
                    if !delta.trim().is_empty() {
                        emit_watch_bus_delta(
                            event.provider.as_deref().unwrap_or("provider"),
                            event.req_id.as_deref().unwrap_or("-"),
                            delta,
                            pane_ui,
                            false,
                        )?;
                    }
                }
            }

            if with_debug_log {
                let debug_log = logs_instance_dir(project_dir, instance).join("debug.log");
                tail_log_all(&debug_log, "debug", &mut debug_log_offset, pane_ui, false)?;
            }
        }
    }
}

fn is_terminal_bus_task_event(event: &AskBusEvent) -> bool {
    if event.event != "done" {
        return false;
    }
    if let Some(status) = event.status.as_deref() {
        return is_terminal_task_status(status);
    }
    event.exit_code.map(|code| code != 0).unwrap_or(false)
}

fn emit_watch_bus_delta(
    source: &str,
    req_id: &str,
    delta: &str,
    pane_ui: bool,
    as_json: bool,
) -> Result<()> {
    if as_json {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "event": "log",
                "source": source,
                "req_id": req_id,
                "line": delta,
            }))?
        );
        return Ok(());
    }

    let normalized = delta.replace('\r', "");
    let mut emitted = false;
    for line in normalized.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        emitted = true;
        if pane_ui {
            println!("[{}] {}", short_watch_source(source), line);
        } else {
            println!("[{}] [STREAM] req_id={} {}", source, req_id, line);
        }
    }
    if !emitted {
        let tail = normalized.trim();
        if !tail.is_empty() {
            if pane_ui {
                println!("[{}] {}", short_watch_source(source), tail);
            } else {
                println!("[{}] [STREAM] req_id={} {}", source, req_id, tail);
            }
        }
    }
    Ok(())
}

fn load_task_by_req_id(
    project_dir: &Path,
    instance: &str,
    req_id: &str,
) -> Result<Option<TaskView>> {
    let exact_path = task_file_for_req_id(project_dir, instance, req_id);
    if exact_path.exists() {
        let raw = fs::read_to_string(&exact_path)
            .with_context(|| format!("read task file failed: {}", exact_path.display()))?;
        let v: Value = serde_json::from_str(&raw)
            .with_context(|| format!("parse task file failed: {}", exact_path.display()))?;
        let task_id = v
            .get("task_id")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                exact_path
                    .file_stem()
                    .and_then(|x| x.to_str())
                    .unwrap_or("unknown")
                    .to_string()
            });
        return Ok(Some(TaskView {
            instance: instance.to_string(),
            task_id,
            req_id: v
                .get("req_id")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
            provider: v
                .get("provider")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
            status: v
                .get("status")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown")
                .to_string(),
            created_at_unix: v.get("created_at_unix").and_then(|x| x.as_u64()),
            started_at_unix: v.get("started_at_unix").and_then(|x| x.as_u64()),
            completed_at_unix: v.get("completed_at_unix").and_then(|x| x.as_u64()),
            exit_code: v
                .get("exit_code")
                .and_then(|x| x.as_i64())
                .map(|x| x as i32),
            reply: v
                .get("reply")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
        }));
    }

    for task in load_tasks_in_instance(project_dir, instance)? {
        if task.req_id.as_deref() == Some(req_id) {
            return Ok(Some(task));
        }
    }

    Ok(None)
}

fn select_watch_req_for_provider(
    project_dir: &Path,
    instance: &str,
    provider: &str,
) -> Result<Option<String>> {
    let mut tasks = load_tasks_in_instance(project_dir, instance)?
        .into_iter()
        .filter(|t| t.provider.as_deref() == Some(provider))
        .filter(|t| {
            t.req_id
                .as_ref()
                .map(|x| !x.trim().is_empty())
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    if tasks.is_empty() {
        return Ok(None);
    }

    tasks.sort_by(|a, b| {
        b.created_at_unix
            .unwrap_or(0)
            .cmp(&a.created_at_unix.unwrap_or(0))
    });

    if let Some(inflight) = tasks.iter().find(|t| is_in_flight_status(&t.status)) {
        return Ok(inflight.req_id.clone());
    }
    Ok(tasks[0].req_id.clone())
}

fn select_watch_req_for_provider_follow(
    project_dir: &Path,
    instance: &str,
    provider: &str,
    done_req_ids: &HashSet<String>,
    follow_started_at: u64,
) -> Result<Option<String>> {
    let mut tasks = load_tasks_in_instance(project_dir, instance)?
        .into_iter()
        .filter(|t| t.provider.as_deref() == Some(provider))
        .filter(|t| {
            t.req_id
                .as_ref()
                .map(|x| !x.trim().is_empty())
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    if tasks.is_empty() {
        return Ok(None);
    }

    tasks.sort_by(|a, b| {
        b.created_at_unix
            .unwrap_or(0)
            .cmp(&a.created_at_unix.unwrap_or(0))
    });

    if let Some(inflight) = tasks.iter().find(|t| {
        is_in_flight_status(&t.status)
            && t.req_id
                .as_ref()
                .map(|rid| !done_req_ids.contains(rid))
                .unwrap_or(false)
    }) {
        return Ok(inflight.req_id.clone());
    }

    for task in tasks {
        let Some(req_id) = task.req_id.clone() else {
            continue;
        };
        if done_req_ids.contains(&req_id) {
            continue;
        }
        if task.created_at_unix.unwrap_or(0) < follow_started_at {
            continue;
        }
        return Ok(Some(req_id));
    }
    Ok(None)
}

fn task_file_for_req_id(project_dir: &Path, instance: &str, req_id: &str) -> PathBuf {
    tasks_instance_dir(project_dir, instance)
        .join(format!("task-{}.json", sanitize_filename(req_id)))
}

fn emit_watch_task(task: &TaskView, as_json: bool, pane_ui: bool) -> Result<()> {
    if as_json {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "event": "task",
                "task": task
            }))?
        );
        return Ok(());
    }

    if pane_ui {
        println!(
            "[task] req_id={} provider={} status={} exit={}",
            task.req_id.clone().unwrap_or_else(|| "-".to_string()),
            task.provider.clone().unwrap_or_else(|| "-".to_string()),
            task.status,
            task.exit_code
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string()),
        );
    } else {
        println!(
            "watch: instance={} req_id={} provider={} status={} exit={} created={} started={} completed={}",
            task.instance,
            task.req_id.clone().unwrap_or_else(|| "-".to_string()),
            task.provider.clone().unwrap_or_else(|| "-".to_string()),
            task.status,
            task.exit_code
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string()),
            task.created_at_unix
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string()),
            task.started_at_unix
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string()),
            task.completed_at_unix
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string()),
        );
    }
    if is_terminal_task_status(&task.status) {
        if let Some(reply) = task.reply.as_deref() {
            if !reply.trim().is_empty() {
                if pane_ui {
                    println!("[reply] {}", reply);
                } else {
                    println!("reply: {}", reply);
                }
            }
        }
    }
    Ok(())
}

fn tail_log_for_req(
    path: &Path,
    req_id: &str,
    source: &str,
    offset: &mut u64,
    pane_ui: bool,
    as_json: bool,
) -> Result<()> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| format!("open log failed: {}", path.display()));
        }
    };

    let len = file.metadata()?.len();
    if *offset > len {
        *offset = 0;
    }

    file.seek(SeekFrom::Start(*offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    *offset = len;
    if buf.is_empty() {
        return Ok(());
    }

    let txt = String::from_utf8_lossy(&buf);
    let mut matched = Vec::<String>::new();
    for line in txt.lines() {
        if !line.contains(req_id) {
            continue;
        }
        matched.push(line.to_string());
    }

    if matched.is_empty() {
        return Ok(());
    }

    if as_json {
        for line in matched {
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "event": "log",
                    "source": source,
                    "path": path.display().to_string(),
                    "line": line
                }))?
            );
        }
        return Ok(());
    }

    let max_lines = watch_max_log_lines();
    let total = matched.len();
    let start = total.saturating_sub(max_lines);
    if start > 0 && !pane_ui {
        println!(
            "[{}] +{} 行省略（可设置 RCCB_WATCH_MAX_LOG_LINES 调整）",
            source, start
        );
    }
    let view = &matched[start..];
    emit_compact_text_lines(source, view, pane_ui);
    Ok(())
}

fn tail_log_all(
    path: &Path,
    source: &str,
    offset: &mut u64,
    pane_ui: bool,
    as_json: bool,
) -> Result<()> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| format!("open log failed: {}", path.display()));
        }
    };

    let len = file.metadata()?.len();
    if *offset > len {
        *offset = 0;
    }

    file.seek(SeekFrom::Start(*offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    *offset = len;
    if buf.is_empty() {
        return Ok(());
    }

    let txt = String::from_utf8_lossy(&buf);
    let lines: Vec<String> = txt
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.to_string())
        .collect();
    if lines.is_empty() {
        return Ok(());
    }

    if as_json {
        for line in lines {
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "event": "log",
                    "source": source,
                    "path": path.display().to_string(),
                    "line": line
                }))?
            );
        }
        return Ok(());
    }

    let max_lines = watch_max_log_lines();
    let start = lines.len().saturating_sub(max_lines);
    emit_compact_text_lines(source, &lines[start..], pane_ui);
    Ok(())
}

fn watch_max_log_lines() -> usize {
    let raw = std::env::var("RCCB_WATCH_MAX_LOG_LINES").unwrap_or_default();
    raw.trim()
        .parse::<usize>()
        .ok()
        .filter(|v| *v > 0)
        .unwrap_or(10)
}

fn emit_compact_text_lines(source: &str, lines: &[String], pane_ui: bool) {
    let mut prev: Option<&str> = None;
    let mut count = 0usize;
    for line in lines {
        let cur = line.as_str();
        match prev {
            Some(p) if p == cur => {
                count += 1;
            }
            Some(p) => {
                print_compact_line(source, p, count, pane_ui);
                prev = Some(cur);
                count = 1;
            }
            None => {
                prev = Some(cur);
                count = 1;
            }
        }
    }
    if let Some(p) = prev {
        print_compact_line(source, p, count, pane_ui);
    }
}

fn print_compact_line(source: &str, line: &str, count: usize, pane_ui: bool) {
    let source = short_watch_source(source);
    let line = if pane_ui {
        compact_watch_line(line)
    } else {
        line.to_string()
    };

    if count <= 1 {
        println!("[{}] {}", source, line);
    } else {
        println!("[{}] {} (x{})", source, line, count);
    }
}

fn emit_watch_pane_header(instance: &str, provider: Option<&str>, req_id: Option<&str>) {
    println!(
        "== RCCB Live == instance={} provider={} req_id={}",
        instance,
        provider.unwrap_or("-"),
        req_id.unwrap_or("-")
    );
}

fn short_watch_source(source: &str) -> &str {
    match source {
        "provider" => "out",
        "debug" => "dbg",
        other => other,
    }
}

fn compact_watch_line(line: &str) -> String {
    line.replace("[STREAM] ", "")
        .replace("[INFO] ", "")
        .trim()
        .to_string()
}

fn is_terminal_task_status(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "completed" | "failed" | "timeout" | "canceled" | "cancelled" | "incomplete" | "rejected"
    )
}

pub fn cmd_stop(project_dir: &Path, instance: &str) -> Result<()> {
    let path = state_path(project_dir, instance);
    if !path.exists() {
        bail!(
            "instance state not found. project={} instance={} path={}",
            project_dir.display(),
            instance,
            path.display()
        );
    }

    let mut state = load_state(&path)?;
    let mut graceful = false;

    if let (Some(host), Some(port), Some(token)) = (
        state.daemon_host.clone(),
        state.daemon_port,
        state.daemon_token.clone(),
    ) {
        if send_shutdown(&host, port, &token, 1.0).is_ok() {
            graceful = true;
        }
    }

    let mut stop_mode = if graceful { "graceful" } else { "kill" };
    if graceful {
        let waited = wait_process_exit(state.pid, Duration::from_secs_f64(2.0));
        if !waited {
            force_kill_process(state.pid);
            stop_mode = "graceful+kill";
        }
    } else {
        force_kill_process(state.pid);
    }

    let stopped = wait_process_exit(state.pid, Duration::from_secs_f64(1.5));
    state.status = if stopped { "stopped" } else { "stopping" }.to_string();
    state.stopped_at_unix = Some(crate::io_utils::now_unix());
    state.last_heartbeat_unix = state.stopped_at_unix.unwrap_or(state.last_heartbeat_unix);
    crate::io_utils::write_state(&path, &state)?;

    println!(
        "stop signal sent for project={} instance={} pid={} mode={} status={}",
        project_dir.display(),
        instance,
        state.pid,
        stop_mode,
        state.status
    );
    Ok(())
}

fn force_kill_process(pid: u32) {
    let mut sys = System::new_all();
    sys.refresh_processes();

    let pid = Pid::from_u32(pid);
    if let Some(process) = sys.process(pid) {
        let _ = process.kill();
    }
}

fn wait_process_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_process_alive(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(80));
    }
    !is_process_alive(pid)
}

pub fn cmd_ping(project_dir: &Path, instance: &str, timeout_s: f64) -> Result<()> {
    let state = load_state(&state_path(project_dir, instance))?;
    let host = state
        .daemon_host
        .ok_or_else(|| anyhow!("missing daemon_host in state"))?;
    let port = state
        .daemon_port
        .ok_or_else(|| anyhow!("missing daemon_port in state"))?;
    let token = state
        .daemon_token
        .ok_or_else(|| anyhow!("missing daemon_token in state"))?;

    let req = json!({
        "type": format!("{}.ping", PROTOCOL_PREFIX),
        "v": PROTOCOL_VERSION,
        "id": "ping",
        "token": token,
    });

    let resp = send_wire_message(&host, port, req, timeout_s)?;
    let msg_type = resp.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if msg_type != format!("{}.pong", PROTOCOL_PREFIX) {
        bail!("unexpected ping response type: {}", msg_type);
    }

    println!("pong: instance={} daemon={}:{}", instance, host, port);
    Ok(())
}

pub fn cmd_cancel(project_dir: &Path, instance: &str, req_id: &str) -> Result<()> {
    let req_id = req_id.trim();
    if req_id.is_empty() {
        bail!("req_id cannot be empty");
    }

    let state = load_state(&state_path(project_dir, instance))?;
    let host = state
        .daemon_host
        .ok_or_else(|| anyhow!("missing daemon_host in state"))?;
    let port = state
        .daemon_port
        .ok_or_else(|| anyhow!("missing daemon_port in state"))?;
    let token = state
        .daemon_token
        .ok_or_else(|| anyhow!("missing daemon_token in state"))?;

    let req = json!({
        "type": format!("{}.cancel", PROTOCOL_PREFIX),
        "v": PROTOCOL_VERSION,
        "id": format!("cancel-{}-{}", std::process::id(), crate::io_utils::now_unix_ms()),
        "token": token,
        "req_id": req_id,
    });

    let value = send_wire_message(&host, port, req, 2.0)?;
    let resp: AskResponse =
        serde_json::from_value(value).context("invalid ask.cancel response payload")?;

    if resp.exit_code == 0 {
        println!(
            "cancel requested: instance={} req_id={}",
            instance,
            resp.req_id.unwrap_or_else(|| req_id.to_string())
        );
        return Ok(());
    }

    bail!(
        "cancel failed: req_id={} reply={}",
        resp.req_id.unwrap_or_else(|| req_id.to_string()),
        resp.reply
    )
}

pub fn cmd_debug(project_dir: &Path, action: crate::cli::DebugAction) -> Result<()> {
    match action {
        crate::cli::DebugAction::On { instance } => cmd_debug_set(project_dir, &instance, "on"),
        crate::cli::DebugAction::Off { instance } => cmd_debug_set(project_dir, &instance, "off"),
        crate::cli::DebugAction::Status { instance } => {
            cmd_debug_set(project_dir, &instance, "status")
        }
    }
}

fn cmd_debug_set(project_dir: &Path, instance: &str, action: &str) -> Result<()> {
    let path = state_path(project_dir, instance);
    if !path.exists() {
        bail!(
            "instance state not found. project={} instance={} path={}",
            project_dir.display(),
            instance,
            path.display()
        );
    }

    let mut state = load_state(&path)?;
    let debug_log_path = logs_instance_dir(project_dir, instance).join("debug.log");

    let mut applied_by_daemon = false;
    if state.status == "running" {
        if let (Some(host), Some(port), Some(token)) = (
            state.daemon_host.clone(),
            state.daemon_port,
            state.daemon_token.clone(),
        ) {
            let req = json!({
                "type": format!("{}.debug", PROTOCOL_PREFIX),
                "v": PROTOCOL_VERSION,
                "id": format!("debug-{}-{}", std::process::id(), crate::io_utils::now_unix_ms()),
                "token": token,
                "action": action,
            });

            if let Ok(value) = send_wire_message(&host, port, req, 2.0) {
                let resp: AskResponse =
                    serde_json::from_value(value).context("invalid ask.debug response payload")?;
                if resp.exit_code != 0 {
                    bail!("debug action failed: {}", resp.reply);
                }
                if action != "status" {
                    state.debug_enabled = action == "on";
                    crate::io_utils::write_state(&path, &state)?;
                }
                let enabled = resp
                    .meta
                    .as_ref()
                    .and_then(|m| m.get("debug_enabled"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(state.debug_enabled);
                println!(
                    "debug={} instance={} via=daemon log={}",
                    if enabled { "on" } else { "off" },
                    instance,
                    debug_log_path.display()
                );
                applied_by_daemon = true;
            }
        }
    }

    if applied_by_daemon {
        return Ok(());
    }

    match action {
        "on" => state.debug_enabled = true,
        "off" => state.debug_enabled = false,
        "status" => {}
        _ => bail!("invalid debug action: {}", action),
    }
    crate::io_utils::write_state(&path, &state)?;

    println!(
        "debug={} instance={} via=state{} log={}",
        if state.debug_enabled { "on" } else { "off" },
        instance,
        if state.status == "running" {
            " (daemon unreachable)"
        } else {
            " (takes effect on next start)"
        },
        debug_log_path.display()
    );

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_ask(
    project_dir: &Path,
    instance: &str,
    provider: &str,
    caller: &str,
    timeout_s: f64,
    quiet: bool,
    stream: bool,
    async_submit: bool,
    req_id: Option<String>,
    message_parts: Vec<String>,
) -> Result<()> {
    let provider = normalize_provider(provider)?;
    if caller.trim().is_empty() {
        bail!("caller cannot be empty");
    }
    if stream && async_submit {
        bail!("--stream and --async cannot be used together");
    }

    let state = load_state(&state_path(project_dir, instance))?;
    let host = state
        .daemon_host
        .ok_or_else(|| anyhow!("missing daemon_host in state"))?;
    let port = state
        .daemon_port
        .ok_or_else(|| anyhow!("missing daemon_port in state"))?;
    let token = state
        .daemon_token
        .ok_or_else(|| anyhow!("missing daemon_token in state"))?;

    let message = if message_parts.is_empty() {
        read_stdin_all()?.trim().to_string()
    } else {
        message_parts.join(" ")
    };

    if message.trim().is_empty() {
        bail!("message is empty");
    }

    let req = json!({
        "type": format!("{}.request", PROTOCOL_PREFIX),
        "v": PROTOCOL_VERSION,
        "id": format!("ask-{}-{}", std::process::id(), crate::io_utils::now_unix_ms()),
        "token": token,
        "provider": provider,
        "work_dir": project_dir.display().to_string(),
        "timeout_s": timeout_s,
        "quiet": quiet,
        "stream": stream,
        "async": async_submit,
        "message": message,
        "caller": caller,
        "req_id": req_id,
    });

    if stream {
        return cmd_ask_stream(&host, port, req, timeout_s.max(1.0) + 10.0);
    }

    let resp = send_wire_message(&host, port, req, timeout_s.max(1.0) + 5.0)?;
    let parsed: AskResponse =
        serde_json::from_value(resp).context("invalid ask.response payload")?;

    if parsed.exit_code == 0 {
        if async_submit {
            let req_id_print = parsed.req_id.unwrap_or_else(|| "-".to_string());
            let provider_print = parsed.provider.unwrap_or_else(|| provider.to_string());
            println!(
                "已提交：req_id={} provider={} instance={}",
                req_id_print, provider_print, instance
            );
            if req_id_print != "-" {
                println!(
                    "watch: rccb --project-dir {} watch --instance {} --req-id {} --with-provider-log",
                    project_dir.display(),
                    instance,
                    req_id_print
                );
            }
            return Ok(());
        }
        if !parsed.reply.is_empty() {
            println!("{}", parsed.reply);
        }
        return Ok(());
    }

    bail!(
        "ask failed: exit_code={} reply={} req_id={}",
        parsed.exit_code,
        parsed.reply,
        parsed.req_id.unwrap_or_else(|| "-".to_string())
    )
}

fn cmd_ask_stream(host: &str, port: u16, req: Value, timeout_s: f64) -> Result<()> {
    let mut reader = connect_and_send(host, port, req, timeout_s)?;
    let mut line = String::new();
    let mut saw_done = false;
    let mut saw_delta = false;
    let mut output_ends_with_newline = true;

    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .context("read stream response failed")?;
        if n == 0 {
            break;
        }

        let value: Value = serde_json::from_str(&line).context("invalid stream json line")?;
        let msg_type = value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if msg_type == format!("{}.event", PROTOCOL_PREFIX) {
            let event: AskEvent =
                serde_json::from_value(value).context("invalid ask.event payload")?;
            match event.event.as_str() {
                "start" => {}
                "delta" => {
                    let delta = event.delta.unwrap_or_default();
                    if !delta.is_empty() {
                        print!("{}", delta);
                        io::stdout().flush().context("flush stream output failed")?;
                        saw_delta = true;
                        output_ends_with_newline = delta.ends_with('\n');
                    }
                }
                "done" => {
                    let exit_code = event.exit_code.unwrap_or(0);
                    let done_reply = event.reply.clone().unwrap_or_default();
                    if !saw_delta {
                        if !done_reply.is_empty() {
                            print!("{}", done_reply);
                            output_ends_with_newline = done_reply.ends_with('\n');
                            io::stdout()
                                .flush()
                                .context("flush stream done output failed")?;
                        }
                    }
                    if saw_delta && !output_ends_with_newline {
                        println!();
                    }
                    if exit_code != 0 {
                        let reply = if done_reply.is_empty() {
                            "流式任务以非零退出码结束".to_string()
                        } else {
                            done_reply
                        };
                        bail!("ask stream failed: exit_code={} reply={}", exit_code, reply);
                    }
                    saw_done = true;
                    break;
                }
                "error" => {
                    let reply = event
                        .reply
                        .unwrap_or_else(|| "流式任务发生错误".to_string());
                    bail!("ask stream failed: {}", reply);
                }
                other => {
                    bail!("unknown stream event: {}", other);
                }
            }
            continue;
        }

        if msg_type == format!("{}.response", PROTOCOL_PREFIX) {
            let parsed: AskResponse =
                serde_json::from_value(value).context("invalid fallback ask.response")?;
            if parsed.exit_code == 0 {
                if !parsed.reply.is_empty() {
                    println!("{}", parsed.reply);
                }
                return Ok(());
            }
            bail!(
                "ask failed: exit_code={} reply={} req_id={}",
                parsed.exit_code,
                parsed.reply,
                parsed.req_id.unwrap_or_else(|| "-".to_string())
            );
        }

        bail!("unexpected stream message type: {}", msg_type);
    }

    if !saw_done {
        bail!("stream ended before done event");
    }

    Ok(())
}

pub fn cmd_send(channel: crate::cli::SendChannel) -> Result<()> {
    match channel {
        crate::cli::SendChannel::Feishu { webhook_url, text } => {
            let chan = FeishuChannel {
                webhook_url,
                client: build_http_client()?,
            };
            chan.send_text(&text)?;
            println!("sent via feishu");
        }
        crate::cli::SendChannel::Telegram {
            bot_token,
            chat_id,
            text,
            message_thread_id,
        } => {
            let chan = TelegramChannel {
                bot_token,
                chat_id,
                message_thread_id,
                client: build_http_client()?,
            };
            chan.send_text(&text)?;
            println!("sent via telegram");
        }
    }
    Ok(())
}

fn send_shutdown(host: &str, port: u16, token: &str, timeout_s: f64) -> Result<()> {
    let req = json!({
        "type": format!("{}.shutdown", PROTOCOL_PREFIX),
        "v": PROTOCOL_VERSION,
        "id": "shutdown",
        "token": token,
    });
    let _resp = send_wire_message(host, port, req, timeout_s)?;
    Ok(())
}

fn resolve_start_debug(project_dir: &Path, instance: &str, cli_debug: bool) -> bool {
    if cli_debug {
        return true;
    }
    let existing_state = state_path(project_dir, instance);
    if !existing_state.exists() {
        return false;
    }
    load_state(&existing_state)
        .map(|s| s.debug_enabled)
        .unwrap_or(false)
}

fn env_debug_enabled() -> bool {
    let raw = match env::var("RCCB_DEBUG") {
        Ok(v) => v,
        Err(_) => return false,
    };
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::path::Path;
    use std::sync::{Mutex, OnceLock};

    use serde_json::json;

    use super::{
        build_debug_watch_command, cleanup_inflight_tasks, compact_watch_line,
        debug_watch_pane_percent, is_in_flight_status, is_terminal_bus_task_event,
        is_terminal_task_status, load_task_by_req_id, orchestrator_guardrail_prompt,
        orchestrator_strict_mode_enabled, provider_start_cmd, resolve_debug_watch_provider,
        select_watch_req_for_provider, select_watch_req_for_provider_follow, split_layout_groups,
        split_percent_for_equal_stack, task_file_for_req_id, watch_bus_enabled,
    };
    use crate::io_utils::{now_unix, now_unix_ms, update_task_status, write_json_pretty};
    use crate::layout::{ensure_project_layout, tasks_instance_dir};
    use crate::types::AskBusEvent;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn status_helpers_match_expected_states() {
        assert!(is_in_flight_status("queued"));
        assert!(is_in_flight_status("running"));
        assert!(!is_in_flight_status("completed"));

        assert!(is_terminal_task_status("completed"));
        assert!(is_terminal_task_status("canceled"));
        assert!(is_terminal_task_status("incomplete"));
        assert!(!is_terminal_task_status("running"));
    }

    #[test]
    fn watch_bus_enabled_respects_env() {
        let _guard = env_lock().lock().expect("lock env");
        std::env::remove_var("RCCB_WATCH_BUS");
        assert!(watch_bus_enabled());

        std::env::set_var("RCCB_WATCH_BUS", "0");
        assert!(!watch_bus_enabled());

        std::env::set_var("RCCB_WATCH_BUS", "false");
        assert!(!watch_bus_enabled());

        std::env::set_var("RCCB_WATCH_BUS", "1");
        assert!(watch_bus_enabled());

        std::env::remove_var("RCCB_WATCH_BUS");
    }

    #[test]
    fn terminal_bus_event_matches_done_status() {
        let mut event = AskBusEvent {
            msg_type: "ask.bus".to_string(),
            v: 1,
            id: "watch-1".to_string(),
            seq: 1,
            ts_unix_ms: 1,
            req_id: Some("req-1".to_string()),
            provider: Some("opencode".to_string()),
            event: "done".to_string(),
            delta: None,
            reply: None,
            status: Some("completed".to_string()),
            exit_code: Some(0),
            meta: None,
        };
        assert!(is_terminal_bus_task_event(&event));

        event.event = "delta".to_string();
        assert!(!is_terminal_bus_task_event(&event));
    }

    #[test]
    fn task_file_path_sanitizes_req_id() {
        let path = task_file_for_req_id(Path::new("/tmp/rccb-x"), "team-a", "req/1 a");
        assert_eq!(
            path.file_name().and_then(|x| x.to_str()),
            Some("task-req_1_a.json")
        );
    }

    #[test]
    fn load_task_by_req_id_supports_exact_and_fallback_lookup() {
        let project = std::env::temp_dir().join(format!("rccb-test-{}", now_unix_ms()));
        let instance = "demo";
        ensure_project_layout(&project).unwrap();
        let task_dir = tasks_instance_dir(&project, instance);
        fs::create_dir_all(&task_dir).unwrap();

        let req_exact = "req-exact";
        let exact_file = task_file_for_req_id(&project, instance, req_exact);
        write_json_pretty(
            &exact_file,
            &json!({
                "task_id": "task-req-exact",
                "req_id": req_exact,
                "provider": "codex",
                "status": "running",
                "created_at_unix": 1
            }),
        )
        .unwrap();

        let req_fallback = "req-fallback";
        let fallback_file = task_dir.join("task-custom-name.json");
        write_json_pretty(
            &fallback_file,
            &json!({
                "task_id": "task-custom-name",
                "req_id": req_fallback,
                "provider": "claude",
                "status": "queued",
                "created_at_unix": 2
            }),
        )
        .unwrap();

        let t1 = load_task_by_req_id(&project, instance, req_exact)
            .unwrap()
            .expect("exact req should exist");
        assert_eq!(t1.req_id.as_deref(), Some(req_exact));
        assert_eq!(t1.provider.as_deref(), Some("codex"));

        let t2 = load_task_by_req_id(&project, instance, req_fallback)
            .unwrap()
            .expect("fallback req should exist");
        assert_eq!(t2.req_id.as_deref(), Some(req_fallback));
        assert_eq!(t2.provider.as_deref(), Some("claude"));

        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn select_watch_req_for_provider_prefers_inflight_then_latest() {
        let project = std::env::temp_dir().join(format!("rccb-watch-provider-{}", now_unix_ms()));
        let instance = "default";
        ensure_project_layout(&project).unwrap();
        let task_dir = tasks_instance_dir(&project, instance);
        fs::create_dir_all(&task_dir).unwrap();

        write_json_pretty(
            &task_dir.join("task-a.json"),
            &json!({
                "task_id":"task-a",
                "req_id":"req-a",
                "provider":"opencode",
                "status":"completed",
                "created_at_unix": 100
            }),
        )
        .unwrap();
        write_json_pretty(
            &task_dir.join("task-b.json"),
            &json!({
                "task_id":"task-b",
                "req_id":"req-b",
                "provider":"opencode",
                "status":"running",
                "created_at_unix": 90
            }),
        )
        .unwrap();

        let req = select_watch_req_for_provider(&project, instance, "opencode")
            .unwrap()
            .unwrap();
        assert_eq!(req, "req-b");

        update_task_status(
            &task_dir.join("task-b.json"),
            "completed",
            None,
            Some(now_unix()),
            Some(0),
            Some("ok"),
        )
        .unwrap();

        let req2 = select_watch_req_for_provider(&project, instance, "opencode")
            .unwrap()
            .unwrap();
        assert_eq!(req2, "req-a");

        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn select_watch_req_for_provider_follow_skips_done_and_old_tasks() {
        let project = std::env::temp_dir().join(format!("rccb-watch-follow-{}", now_unix_ms()));
        let instance = "default";
        ensure_project_layout(&project).unwrap();
        let task_dir = tasks_instance_dir(&project, instance);
        fs::create_dir_all(&task_dir).unwrap();

        write_json_pretty(
            &task_dir.join("task-old.json"),
            &json!({
                "task_id":"task-old",
                "req_id":"req-old",
                "provider":"opencode",
                "status":"completed",
                "created_at_unix": 10
            }),
        )
        .unwrap();
        write_json_pretty(
            &task_dir.join("task-new-running.json"),
            &json!({
                "task_id":"task-new-running",
                "req_id":"req-new-running",
                "provider":"opencode",
                "status":"running",
                "created_at_unix": 200
            }),
        )
        .unwrap();

        let mut done = HashSet::<String>::new();
        done.insert("req-new-running".to_string());

        let req = select_watch_req_for_provider_follow(&project, instance, "opencode", &done, 100)
            .unwrap();
        assert_eq!(req, None);

        done.clear();
        let req2 = select_watch_req_for_provider_follow(&project, instance, "opencode", &done, 100)
            .unwrap()
            .unwrap();
        assert_eq!(req2, "req-new-running");

        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn split_layout_groups_matches_shortcut_rules() {
        let exec3 = vec!["b".to_string(), "c".to_string(), "d".to_string()];
        let (l3, r3) = split_layout_groups(&exec3);
        assert!(l3.is_empty());
        assert_eq!(r3, exec3);

        let exec4 = vec![
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
            "e".to_string(),
        ];
        let (l4, r4) = split_layout_groups(&exec4);
        assert_eq!(l4, vec!["b".to_string()]);
        assert_eq!(r4, vec!["c".to_string(), "d".to_string(), "e".to_string()]);
    }

    #[test]
    fn split_percent_for_equal_stack_matches_equal_distribution() {
        assert_eq!(split_percent_for_equal_stack(3, 1), 67);
        assert_eq!(split_percent_for_equal_stack(3, 2), 50);

        assert_eq!(split_percent_for_equal_stack(4, 1), 75);
        assert_eq!(split_percent_for_equal_stack(4, 2), 67);
        assert_eq!(split_percent_for_equal_stack(4, 3), 50);
    }

    #[test]
    fn resolve_debug_watch_provider_defaults_to_all() {
        let _guard = env_lock().lock().unwrap();
        let old = std::env::var("RCCB_DEBUG_WATCH_PROVIDER").ok();
        unsafe {
            std::env::remove_var("RCCB_DEBUG_WATCH_PROVIDER");
        }
        let providers = vec![
            "claude".to_string(),
            "gemini".to_string(),
            "opencode".to_string(),
        ];
        let resolved = resolve_debug_watch_provider(&providers);
        if let Some(v) = old {
            unsafe {
                std::env::set_var("RCCB_DEBUG_WATCH_PROVIDER", v);
            }
        }
        assert_eq!(resolved, None);
    }

    #[test]
    fn resolve_debug_watch_provider_honors_env_if_present_in_list() {
        let _guard = env_lock().lock().unwrap();
        let old = std::env::var("RCCB_DEBUG_WATCH_PROVIDER").ok();
        unsafe {
            std::env::set_var("RCCB_DEBUG_WATCH_PROVIDER", "opencode");
        }
        let providers = vec![
            "claude".to_string(),
            "gemini".to_string(),
            "opencode".to_string(),
        ];
        let resolved = resolve_debug_watch_provider(&providers);
        if let Some(v) = old {
            unsafe {
                std::env::set_var("RCCB_DEBUG_WATCH_PROVIDER", v);
            }
        } else {
            unsafe {
                std::env::remove_var("RCCB_DEBUG_WATCH_PROVIDER");
            }
        }
        assert_eq!(resolved, Some("opencode".to_string()));
    }

    #[test]
    fn debug_watch_pane_percent_uses_default_and_clamp() {
        let _guard = env_lock().lock().unwrap();
        let old = std::env::var("RCCB_DEBUG_WATCH_PANE_PERCENT").ok();

        unsafe {
            std::env::remove_var("RCCB_DEBUG_WATCH_PANE_PERCENT");
        }
        assert_eq!(debug_watch_pane_percent(), 25);

        unsafe {
            std::env::set_var("RCCB_DEBUG_WATCH_PANE_PERCENT", "2");
        }
        assert_eq!(debug_watch_pane_percent(), 10);

        unsafe {
            std::env::set_var("RCCB_DEBUG_WATCH_PANE_PERCENT", "120");
        }
        assert_eq!(debug_watch_pane_percent(), 80);

        if let Some(v) = old {
            unsafe {
                std::env::set_var("RCCB_DEBUG_WATCH_PANE_PERCENT", v);
            }
        } else {
            unsafe {
                std::env::remove_var("RCCB_DEBUG_WATCH_PANE_PERCENT");
            }
        }
    }

    #[test]
    fn build_debug_watch_command_uses_global_pane_ui_mode() {
        let cmd = build_debug_watch_command(Path::new("/tmp/rccb-proj"), "default", None)
            .expect("debug watch command");
        assert!(cmd.contains("--pane-ui"));
        assert!(cmd.contains("--with-provider-log"));
        assert!(cmd.contains("--with-debug-log"));
        assert!(cmd.contains("--timeout-s 0"));
        assert!(cmd.contains("--all"));
    }

    #[test]
    fn build_debug_watch_command_can_scope_specific_provider() {
        let cmd = build_debug_watch_command(Path::new("/tmp/rccb-proj"), "default", Some("codex"))
            .expect("debug watch command");
        assert!(cmd.contains("--provider"));
        assert!(cmd.contains("codex"));
    }

    #[test]
    fn orchestrator_guardrail_prompt_mentions_delegate_only_rules() {
        let prompt =
            orchestrator_guardrail_prompt("claude", &["codex".to_string(), "gemini".to_string()]);
        assert!(prompt.contains("不要自己执行 bash 命令"));
        assert!(prompt.contains("codex, gemini"));
        assert!(prompt.contains("--caller claude"));
        assert!(prompt.contains("RCCB_RESULT"));
    }

    #[test]
    fn orchestrator_strict_mode_defaults_on_when_executors_exist() {
        let _guard = env_lock().lock().unwrap();
        let old = std::env::var("RCCB_ORCHESTRATOR_STRICT").ok();
        unsafe {
            std::env::remove_var("RCCB_ORCHESTRATOR_STRICT");
        }
        assert!(orchestrator_strict_mode_enabled(&["codex".to_string()]));
        assert!(!orchestrator_strict_mode_enabled(&[]));
        if let Some(v) = old {
            unsafe {
                std::env::set_var("RCCB_ORCHESTRATOR_STRICT", v);
            }
        } else {
            unsafe {
                std::env::remove_var("RCCB_ORCHESTRATOR_STRICT");
            }
        }
    }

    #[test]
    fn compact_watch_line_strips_common_prefixes() {
        let line = compact_watch_line("[STREAM] req_id=req-1 hello world");
        assert_eq!(line, "req_id=req-1 hello world");
    }

    #[test]
    fn provider_start_cmd_uses_raw_provider_command() {
        let _guard = env_lock().lock().unwrap();
        let old_ccb = std::env::var("RCCB_USE_BRIDGE_PROVIDER_LAUNCH").ok();
        unsafe {
            std::env::remove_var("RCCB_USE_BRIDGE_PROVIDER_LAUNCH");
        }
        let cmd = provider_start_cmd(Path::new("/tmp/rccb-proj"), "default", "codex");
        if let Some(v) = old_ccb {
            unsafe {
                std::env::set_var("RCCB_USE_BRIDGE_PROVIDER_LAUNCH", v);
            }
        } else {
            unsafe {
                std::env::remove_var("RCCB_USE_BRIDGE_PROVIDER_LAUNCH");
            }
        }
        assert!(cmd.contains("codex"));
        assert!(!cmd.contains("tail -n0 -F"));
    }

    #[test]
    fn cleanup_inflight_tasks_marks_running_and_queued_canceled() {
        let project = std::env::temp_dir().join(format!("rccb-clean-{}", now_unix_ms()));
        let instance = "default";
        ensure_project_layout(&project).unwrap();
        let task_dir = tasks_instance_dir(&project, instance);
        fs::create_dir_all(&task_dir).unwrap();

        let running = task_dir.join("task-running.json");
        let queued = task_dir.join("task-queued.json");
        let done = task_dir.join("task-done.json");
        write_json_pretty(
            &running,
            &json!({"task_id":"task-running","req_id":"r1","status":"running"}),
        )
        .unwrap();
        write_json_pretty(
            &queued,
            &json!({"task_id":"task-queued","req_id":"q1","status":"queued"}),
        )
        .unwrap();
        write_json_pretty(
            &done,
            &json!({"task_id":"task-done","req_id":"d1","status":"completed"}),
        )
        .unwrap();

        let cleaned = cleanup_inflight_tasks(&project, instance).unwrap();
        assert_eq!(cleaned, 2);

        let r: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&running).unwrap()).unwrap();
        let q: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&queued).unwrap()).unwrap();
        let d: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&done).unwrap()).unwrap();
        assert_eq!(r.get("status").and_then(|x| x.as_str()), Some("canceled"));
        assert_eq!(q.get("status").and_then(|x| x.as_str()), Some("canceled"));
        assert_eq!(d.get("status").and_then(|x| x.as_str()), Some("completed"));

        let _ = fs::remove_dir_all(&project);
    }
}

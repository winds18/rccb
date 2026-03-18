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
    ensure_project_layout, launcher_feed_dir, launcher_feed_path, launcher_meta_path, lock_path,
    logs_instance_dir, rccb_dir, sanitize_filename, session_instance_dir, state_path,
    tasks_instance_dir, tasks_root_dir, tmp_instance_dir,
};
use crate::orchestrator_lock::{clear_inflight, load_inflight, mark_inflight};
use crate::protocol::{connect_and_send, send_wire_message};
use crate::provider::{
    dispatch_text_to_pane, PaneBackend as ProviderPaneBackend, PaneDispatchTarget,
};
use crate::types::{AskBusEvent, AskEvent, AskResponse, InstanceState};

const RCCB_MANAGED_BEGIN: &str = "<!-- RCCB:BEGIN MANAGED -->";
const RCCB_MANAGED_END: &str = "<!-- RCCB:END MANAGED -->";
const RCCB_USER_BEGIN: &str = "<!-- RCCB:BEGIN USER -->";
const RCCB_USER_END: &str = "<!-- RCCB:END USER -->";
const SHORTCUT_DEFAULT_PROVIDERS: &[&str] = &["claude", "opencode", "gemini", "codex", "droid"];

pub fn cmd_init(project_dir: &Path, force: bool) -> Result<()> {
    let mode = if force {
        BootstrapMode::RefreshGenerated
    } else {
        BootstrapMode::MissingOnly
    };
    let providers = bootstrap_providers_for_init(project_dir);
    let bootstrap = ensure_project_bootstrap(project_dir, mode, &providers)?;

    println!("初始化完成：{}", rccb_dir(project_dir).display());
    println!("配置模板：{}", bootstrap.config_path.display());
    for p in bootstrap.profile_templates {
        println!("native profile 模板：{}", p.display());
    }
    for p in bootstrap.wrapper_scripts {
        println!("provider 包装脚本：{}", p.display());
    }
    for p in bootstrap.provider_support_files {
        println!("provider 支持文件：{}", p.display());
    }
    for p in bootstrap.rule_templates {
        println!("规则模板：{}", p.display());
    }
    Ok(())
}

fn bootstrap_providers_for_init(project_dir: &Path) -> Vec<String> {
    let installed = shortcut_installed_default_providers(project_dir);
    if installed.is_empty() {
        SHORTCUT_DEFAULT_PROVIDERS
            .iter()
            .map(|x| x.to_string())
            .collect()
    } else {
        installed
    }
}

fn write_native_profile_templates(
    project_dir: &Path,
    mode: BootstrapMode,
    providers: &[String],
) -> Result<Vec<PathBuf>> {
    let profile_dir = rccb_dir(project_dir).join("providers");
    fs::create_dir_all(&profile_dir)?;

    let mut written = Vec::new();
    for provider in providers {
        let path = profile_dir.join(format!("{}.example.json", provider));
        if path.exists() && matches!(mode, BootstrapMode::MissingOnly) {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BootstrapMode {
    MissingOnly,
    RefreshGenerated,
}

struct ProjectBootstrapSummary {
    config_path: PathBuf,
    profile_templates: Vec<PathBuf>,
    wrapper_scripts: Vec<PathBuf>,
    provider_support_files: Vec<PathBuf>,
    rule_templates: Vec<PathBuf>,
}

fn ensure_project_bootstrap(
    project_dir: &Path,
    mode: BootstrapMode,
    providers: &[String],
) -> Result<ProjectBootstrapSummary> {
    ensure_project_layout(project_dir)?;
    let config_path = write_config_template(project_dir, mode, providers)?;
    let profile_templates = write_native_profile_templates(project_dir, mode, providers)?;
    let wrapper_scripts = write_provider_launch_wrappers(project_dir, mode, providers)?;
    let provider_support_files = write_provider_support_files(project_dir, mode, providers)?;
    let rule_templates = ensure_project_rule_bootstrap(project_dir, mode, providers)?;
    Ok(ProjectBootstrapSummary {
        config_path,
        profile_templates,
        wrapper_scripts,
        provider_support_files,
        rule_templates,
    })
}

fn write_provider_support_files(
    project_dir: &Path,
    mode: BootstrapMode,
    providers: &[String],
) -> Result<Vec<PathBuf>> {
    let provider_dir = rccb_dir(project_dir).join("providers");
    fs::create_dir_all(&provider_dir)?;

    let mut written = Vec::new();
    let claude_settings_path = project_dir.join(".claude").join("settings.local.json");
    if providers.iter().any(|p| p == "claude") || claude_settings_path.exists() {
        if write_claude_settings_file(project_dir, &claude_settings_path, mode)? {
            written.push(claude_settings_path);
        }
    }

    if providers.iter().any(|p| p == "gemini") {
        let gemini_trusted_folders_path = provider_dir.join("gemini.trustedFolders.json");
        if !(gemini_trusted_folders_path.exists() && matches!(mode, BootstrapMode::MissingOnly)) {
            let trusted = json!({
                project_dir.display().to_string(): "TRUST_FOLDER"
            });
            write_json_pretty(&gemini_trusted_folders_path, &trusted)?;
            written.push(gemini_trusted_folders_path);
        }
    }

    if providers.iter().any(|p| p == "droid") {
        let droid_settings_path = project_dir.join(".factory").join("settings.local.json");
        if !(droid_settings_path.exists() && matches!(mode, BootstrapMode::MissingOnly)) {
            let settings = json!({
                "autonomyMode": "auto-high"
            });
            write_json_pretty(&droid_settings_path, &settings)?;
            written.push(droid_settings_path);
        }
    }

    Ok(written)
}

fn write_claude_settings_file(
    project_dir: &Path,
    path: &Path,
    mode: BootstrapMode,
) -> Result<bool> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let desired = build_claude_settings_local(project_dir);
    let output = if path.exists() && matches!(mode, BootstrapMode::MissingOnly) {
        merge_claude_settings_local(path, &desired)?
    } else {
        desired
    };

    let should_write = match fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(existing) => existing != output,
            Err(_) => true,
        },
        Err(_) => true,
    };

    if should_write {
        write_json_pretty(path, &output)?;
    }

    Ok(should_write)
}

fn build_claude_settings_local(project_dir: &Path) -> Value {
    json!({
        "permissions": {
            "allow": claude_rccb_allowed_tools(project_dir)
        }
    })
}

fn merge_claude_settings_local(path: &Path, desired: &Value) -> Result<Value> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("读取 Claude 项目设置失败：{}", path.display()))?;
    let mut current: Value = match serde_json::from_str::<Value>(&raw) {
        Ok(v) if v.is_object() => v,
        _ => return Ok(desired.clone()),
    };

    let allow_values = desired
        .get("permissions")
        .and_then(|v| v.get("allow"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let root = match current.as_object_mut() {
        Some(v) => v,
        None => return Ok(desired.clone()),
    };
    let permissions = root
        .entry("permissions".to_string())
        .or_insert_with(|| json!({}));
    if !permissions.is_object() {
        *permissions = json!({});
    }

    let permissions_obj = permissions
        .as_object_mut()
        .ok_or_else(|| anyhow!("Claude permissions 节点不是对象"))?;
    let allow = permissions_obj
        .entry("allow".to_string())
        .or_insert_with(|| json!([]));
    if !allow.is_array() {
        *allow = json!([]);
    }

    let allow_array = allow
        .as_array_mut()
        .ok_or_else(|| anyhow!("Claude permissions.allow 节点不是数组"))?;
    let mut seen: HashSet<String> = allow_array
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    for item in allow_values {
        if let Some(text) = item.as_str() {
            if seen.insert(text.to_string()) {
                allow_array.push(Value::String(text.to_string()));
            }
        }
    }

    Ok(current)
}

fn claude_rccb_allowed_tools(project_dir: &Path) -> Vec<String> {
    let debug_abs = project_dir.join("target").join("debug").join("rccb");
    let release_abs = project_dir.join("target").join("release").join("rccb");
    let binaries = vec![
        "rccb".to_string(),
        "./target/debug/rccb".to_string(),
        "'./target/debug/rccb'".to_string(),
        "./target/release/rccb".to_string(),
        "'./target/release/rccb'".to_string(),
        debug_abs.display().to_string(),
        format!("'{}'", debug_abs.display()),
        release_abs.display().to_string(),
        format!("'{}'", release_abs.display()),
    ];
    let prefixes = vec![
        String::new(),
        "RCCB_ASK_ASYNC_STDOUT=minimal ".to_string(),
        "RCCB_ASK_ASYNC_STDOUT=full ".to_string(),
        "RCCB_ASK_ASYNC_STDOUT=* ".to_string(),
    ];
    let mut allow = vec!["WebSearch".to_string()];
    let mut seen = HashSet::new();
    seen.insert("WebSearch".to_string());

    for prefix in prefixes {
        for bin in &binaries {
            let pattern = format!("Bash({}{}:*)", prefix, bin);
            if seen.insert(pattern.clone()) {
                allow.push(pattern);
            }
        }
    }

    allow
}

fn write_config_template(
    project_dir: &Path,
    mode: BootstrapMode,
    providers: &[String],
) -> Result<PathBuf> {
    let config_path = rccb_dir(project_dir).join("config.example.json");
    if config_path.exists() && matches!(mode, BootstrapMode::MissingOnly) {
        return Ok(config_path);
    }

    let mut specialties = serde_json::Map::new();
    if providers.iter().any(|p| p == "claude") {
        specialties.insert("claude".to_string(), json!("编排者"));
    }
    if providers.iter().any(|p| p == "opencode") {
        specialties.insert("opencode".to_string(), json!("编码者"));
    }
    if providers.iter().any(|p| p == "gemini") {
        specialties.insert("gemini".to_string(), json!("调研者"));
    }
    if providers.iter().any(|p| p == "droid") {
        specialties.insert("droid".to_string(), json!("文档记录者"));
    }
    if providers.iter().any(|p| p == "codex") {
        specialties.insert("codex".to_string(), json!("代码审计者"));
    }
    let research_validation_rule =
        if providers.iter().any(|p| p == "gemini") && providers.iter().any(|p| p == "codex") {
            "涉及外部事实时先由 gemini 调研，再由 codex 复核关键结论后再采纳"
        } else if providers.iter().any(|p| p == "gemini") {
            "涉及外部事实时先由 gemini 做至少两轮调研；若未启用 codex，请在采纳前人工复核关键事实"
        } else {
            "当前 provider 集合未启用专门调研链路，请按需补充调研与复核执行者"
        };

    let template = json!({
        "project": project_dir.display().to_string(),
        "instances": {
            "default": {
                "heartbeat_secs": 5,
                "listen": "127.0.0.1:0",
                "debug": false,
                "providers": providers,
                "orchestration_rule": "首个 provider 作为编排者，其余 provider 作为执行者",
                "default_specialties": specialties,
                "research_validation_rule": research_validation_rule
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
    Ok(config_path)
}

fn write_provider_launch_wrappers(
    project_dir: &Path,
    mode: BootstrapMode,
    providers: &[String],
) -> Result<Vec<PathBuf>> {
    let bin_dir = rccb_dir(project_dir).join("bin");
    fs::create_dir_all(&bin_dir)?;

    let mut written = Vec::new();
    for provider in providers {
        let path = bin_dir.join(provider);
        if path.exists() && matches!(mode, BootstrapMode::MissingOnly) {
            continue;
        }
        write_wrapper_script(&path, provider)?;
        written.push(path);
    }
    Ok(written)
}

fn refresh_legacy_provider_wrappers(
    project_dir: &Path,
    providers: &[String],
) -> Result<Vec<PathBuf>> {
    let bin_dir = rccb_dir(project_dir).join("bin");
    if !bin_dir.exists() {
        return Ok(Vec::new());
    }

    let mut refreshed = Vec::new();
    for provider in providers {
        let path = bin_dir.join(provider.trim().to_ascii_lowercase());
        if !provider_wrapper_needs_refresh(&path)? {
            continue;
        }
        write_wrapper_script(&path, provider)?;
        refreshed.push(path);
    }
    Ok(refreshed)
}

fn provider_wrapper_needs_refresh(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("读取 provider wrapper 失败：{}", path.display()))?;
    let first_line = raw.lines().next().unwrap_or_default();
    if first_line.contains("zsh") {
        return Ok(true);
    }
    if raw.contains("[[ ") || raw.contains("cmd=(") || raw.contains("cmd+=(") {
        return Ok(true);
    }
    Ok(false)
}

fn write_wrapper_script(path: &Path, provider: &str) -> Result<()> {
    let script = build_provider_wrapper_script(provider)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, script.as_bytes())
        .with_context(|| format!("写入 provider 包装脚本失败：{}", path.display()))?;
    set_executable(path)?;
    Ok(())
}

fn build_provider_wrapper_script(provider: &str) -> Result<String> {
    let base = provider.trim().to_ascii_lowercase();
    let script = match base.as_str() {
        "claude" => {
            r#"#!/usr/bin/env sh
set -eu
project_root="${RCCB_PROJECT_DIR:-$PWD}"
cd "$project_root"
role="${RCCB_PROVIDER_ROLE:-executor}"
agent="${RCCB_PROVIDER_AGENT:-}"
if [ "$role" = "orchestrator" ]; then
  if [ -n "$agent" ]; then
    exec claude \
      --setting-sources user,project,local \
      --permission-mode default \
      --allowedTools "Read Grep Glob LS Task Bash(rccb:*) Bash(./target/debug/rccb:*) Bash($project_root/target/debug/rccb:*)" \
      --disallowedTools "Edit MultiEdit Write NotebookEdit" \
      --agent "$agent" \
      "$@"
  fi
  exec claude \
    --setting-sources user,project,local \
    --permission-mode default \
    --allowedTools "Read Grep Glob LS Task Bash(rccb:*) Bash(./target/debug/rccb:*) Bash($project_root/target/debug/rccb:*)" \
    --disallowedTools "Edit MultiEdit Write NotebookEdit" \
    "$@"
fi
if [ -n "$agent" ]; then
  exec claude \
    --setting-sources user,project,local \
    --permission-mode bypassPermissions \
    --dangerously-skip-permissions \
    --agent "$agent" \
    "$@"
fi
exec claude \
  --setting-sources user,project,local \
  --permission-mode bypassPermissions \
  --dangerously-skip-permissions \
  "$@"
"#
        }
        "opencode" => {
            r#"#!/usr/bin/env sh
set -eu
project_root="${RCCB_PROJECT_DIR:-$PWD}"
cd "$project_root"
agent="${RCCB_PROVIDER_AGENT:-}"
if [ -n "$agent" ]; then
  exec opencode "$project_root" --agent "$agent" "$@"
fi
exec opencode "$project_root" "$@"
"#
        }
        "codex" => {
            r#"#!/usr/bin/env sh
set -eu
project_root="${RCCB_PROJECT_DIR:-$PWD}"
cd "$project_root"
role="${RCCB_PROVIDER_ROLE:-executor}"
if [ "$role" = "orchestrator" ]; then
  exec codex --cd "$project_root" -a on-request -s workspace-write "$@"
fi
exec codex --cd "$project_root" -a never -s workspace-write "$@"
"#
        }
        "gemini" => {
            r#"#!/usr/bin/env sh
set -eu
project_root="${RCCB_PROJECT_DIR:-$PWD}"
cd "$project_root"
role="${RCCB_PROVIDER_ROLE:-executor}"
trusted_folders_path="${RCCB_GEMINI_TRUSTED_FOLDERS_PATH:-$project_root/.rccb/providers/gemini.trustedFolders.json}"
export GEMINI_CLI_TRUSTED_FOLDERS_PATH="$trusted_folders_path"
if [ "$role" = "orchestrator" ]; then
  exec gemini --approval-mode default "$@"
fi
exec gemini --approval-mode yolo "$@"
"#
        }
        "droid" => {
            r#"#!/usr/bin/env sh
set -eu
project_root="${RCCB_PROJECT_DIR:-$PWD}"
cd "$project_root"
role="${RCCB_PROVIDER_ROLE:-executor}"
droid_settings_path="${RCCB_DROID_SETTINGS_PATH:-$project_root/.factory/settings.local.json}"
if [ "$role" = "orchestrator" ]; then
  if [ -f "$droid_settings_path" ]; then
    exec droid --settings "$droid_settings_path" "$@"
  fi
  exec droid "$@"
fi
if [ -f "$droid_settings_path" ]; then
  exec droid --skip-permissions-unsafe --settings "$droid_settings_path" "$@"
fi
exec droid --skip-permissions-unsafe "$@"
"#
        }
        other => bail!("unsupported provider wrapper: {}", other),
    };
    Ok(script.to_string())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuleFileKind {
    ManagedMarkdown,
    PlainMarkdown,
}

struct RuleFileSpec {
    path: PathBuf,
    contents: String,
    kind: RuleFileKind,
}

fn ensure_project_rule_bootstrap(
    project_dir: &Path,
    mode: BootstrapMode,
    providers: &[String],
) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for spec in build_rule_file_specs(project_dir, providers) {
        let rendered = render_project_bootstrap_content(project_dir, &spec.contents);
        let changed = match spec.kind {
            RuleFileKind::ManagedMarkdown => {
                ensure_managed_markdown_file(&spec.path, &rendered, mode)?
            }
            RuleFileKind::PlainMarkdown => ensure_plain_markdown_file(&spec.path, &rendered, mode)?,
        };
        if changed {
            written.push(spec.path);
        }
    }
    Ok(written)
}

fn render_project_bootstrap_content(project_dir: &Path, contents: &str) -> String {
    contents.replace("rccb --project-dir .", &project_rccb_command(project_dir))
}

fn current_rccb_binary_hint(project_dir: &Path) -> String {
    if let Ok(exe) = env::current_exe() {
        if let Ok(rel) = exe.strip_prefix(project_dir) {
            let rel_display = rel.display().to_string();
            if !rel_display.is_empty() {
                return format!("./{}", rel_display);
            }
        }
        return exe.display().to_string();
    }
    "rccb".to_string()
}

fn project_rccb_command(project_dir: &Path) -> String {
    format!(
        "{} --project-dir {}",
        shell_quote(&current_rccb_binary_hint(project_dir)),
        shell_quote(&project_dir.display().to_string())
    )
}

fn build_rule_file_specs(project_dir: &Path, providers: &[String]) -> Vec<RuleFileSpec> {
    let mut specs = vec![RuleFileSpec {
        path: project_dir.join("AGENTS.md"),
        contents: build_agents_rules_markdown(providers),
        kind: RuleFileKind::ManagedMarkdown,
    }];

    if providers.iter().any(|p| p == "claude") {
        specs.push(RuleFileSpec {
            path: project_dir.join("CLAUDE.md"),
            contents: build_claude_rules_markdown(providers),
            kind: RuleFileKind::ManagedMarkdown,
        });
    }
    if providers.iter().any(|p| p == "gemini") {
        specs.push(RuleFileSpec {
            path: project_dir.join("GEMINI.md"),
            contents: build_gemini_rules_markdown(providers),
            kind: RuleFileKind::ManagedMarkdown,
        });
    }
    if providers.iter().any(|p| p == "codex") {
        specs.push(RuleFileSpec {
            path: project_dir
                .join(".agents")
                .join("skills")
                .join("rccb-delegate")
                .join("SKILL.md"),
            contents: build_agents_delegate_skill_markdown(providers),
            kind: RuleFileKind::PlainMarkdown,
        });
        specs.push(RuleFileSpec {
            path: project_dir
                .join(".agents")
                .join("skills")
                .join("rccb-audit")
                .join("SKILL.md"),
            contents: build_agents_audit_skill_markdown(),
            kind: RuleFileKind::PlainMarkdown,
        });
        if providers.iter().any(|p| p == "gemini") {
            specs.push(RuleFileSpec {
                path: project_dir
                    .join(".agents")
                    .join("skills")
                    .join("rccb-research-verify")
                    .join("SKILL.md"),
                contents: build_agents_research_verify_skill_markdown(),
                kind: RuleFileKind::PlainMarkdown,
            });
        }
    }
    if providers.iter().any(|p| p == "opencode") {
        specs.push(RuleFileSpec {
            path: project_dir
                .join(".opencode")
                .join("skills")
                .join("rccb-delegate")
                .join("SKILL.md"),
            contents: build_opencode_delegate_skill_markdown(providers),
            kind: RuleFileKind::PlainMarkdown,
        });
    }
    if providers.iter().any(|p| p == "droid") {
        specs.push(RuleFileSpec {
            path: project_dir
                .join(".factory")
                .join("skills")
                .join("rccb-delegate")
                .join("SKILL.md"),
            contents: build_factory_delegate_skill_markdown(providers),
            kind: RuleFileKind::PlainMarkdown,
        });
    }

    if providers.iter().any(|p| p == "opencode") {
        specs.push(RuleFileSpec {
            path: project_dir
                .join(".opencode")
                .join("commands")
                .join("rccb-code.md"),
            contents: build_opencode_command_markdown(
                "把编码任务委派给 opencode",
                "当任务需要改代码、修复实现、运行测试或联调时，优先通过 RCCB 把任务委派给 `opencode`。\n\
任务内容：$ARGUMENTS\n\n\
请执行：\n\
`rccb --project-dir . ask --instance default --provider opencode --caller claude \"$ARGUMENTS\"`\n\n\
如果任务依赖外部事实，请先改用 `rccb-research`。",
            ),
            kind: RuleFileKind::PlainMarkdown,
        });
        specs.push(RuleFileSpec {
            path: project_dir
                .join(".opencode")
                .join("commands")
                .join("rccb-research.md"),
            contents: build_opencode_command_markdown(
                "先让 gemini 调研，再让 codex 复核",
                "当任务涉及联网调研、网页资料、版本差异或事实核验时：\n\
1. 先委派给 `gemini` 做至少两轮调研与交叉验证。\n\
2. 再把关键结论委派给 `codex` 复核。\n\
3. 复核完成前，不要把调研结论当作最终依据。\n\n\
任务内容：$ARGUMENTS",
            ),
            kind: RuleFileKind::PlainMarkdown,
        });
        specs.push(RuleFileSpec {
            path: project_dir
                .join(".opencode")
                .join("agents")
                .join("coder.md"),
            contents: build_opencode_agent_markdown(
                "coder",
                "默认编码者。优先承担实现、修复、重构、测试与联调。",
                &[
                    "优先处理代码实现、缺陷修复、测试执行和工程联调。",
                    "不要把自己当作编排者；如果需要其他执行者配合，应通过 RCCB 委派。",
                    "如果任务依赖外部事实或资料，提醒编排者先走 gemini 调研和 codex 复核链路。",
                ],
            ),
            kind: RuleFileKind::PlainMarkdown,
        });
        specs.push(RuleFileSpec {
            path: project_dir
                .join(".opencode")
                .join("agents")
                .join("auditor.md"),
            contents: build_opencode_agent_markdown(
                "auditor",
                "默认代码审计者。优先承担风险分析、边界检查和调研复核。",
                &[
                    "优先检查代码风险、边界条件、回归点和缺失测试。",
                    "对于 gemini 返回的调研结果，重点复核事实冲突、过期信息、落地风险和遗漏约束。",
                    "输出优先给出结论、风险等级和需要补充验证的点。",
                ],
            ),
            kind: RuleFileKind::PlainMarkdown,
        });
    }

    if providers.iter().any(|p| p == "claude") {
        specs.push(RuleFileSpec {
            path: project_dir
                .join(".claude")
                .join("agents")
                .join("orchestrator.md"),
            contents: build_claude_agent_markdown(
                "orchestrator",
                "默认编排者，只负责思考、拆解、委派、验收与汇总。",
                &[
                    "不要自己执行 bash、修改文件或运行测试。",
                    "执行任务优先通过对应的 Claude 委派子代理派出，不要让主编排者直接下场执行。",
                    "实现优先派给 opencode，调研优先派给 gemini，文档优先派给 droid，审计优先派给 codex。",
                    "涉及外部事实时，先让 gemini 做至少两轮调研，再让 codex 复核关键结论。",
                ],
            ),
            kind: RuleFileKind::PlainMarkdown,
        });
        specs.push(RuleFileSpec {
            path: project_dir
                .join(".claude")
                .join("agents")
                .join("reviewer.md"),
            contents: build_claude_agent_markdown(
                "reviewer",
                "默认复核者，优先承担代码审计、风险分析和调研结论核验。",
                &[
                    "优先识别行为回归、边界条件、风险点与缺失测试。",
                    "对于 gemini 的调研结论，要明确指出可采纳项、待验证项和冲突项。",
                    "输出时优先给出结论，再给依据与剩余风险。",
                ],
            ),
            kind: RuleFileKind::PlainMarkdown,
        });
    }

    if providers.iter().any(|p| p == "claude") && providers.iter().any(|p| p == "opencode") {
        specs.push(RuleFileSpec {
            path: project_dir
                .join(".claude")
                .join("commands")
                .join("rccb-code.md"),
            contents: build_claude_command_markdown(
                "委派编码任务给 opencode",
                "使用 RCCB 把实现、改代码、运行测试、联调修复等任务委派给 `opencode`。\n\
任务内容：$ARGUMENTS\n\n\
优先通过 `delegate-coder` 子代理完成派单，主编排者不要直接下场执行。\n\
请直接异步委派，前台只保留最小提交信息，不要自己轮询状态：\n\
`RCCB_ASK_ASYNC_STDOUT=minimal rccb --project-dir . ask --instance default --provider opencode --caller claude --async \"$ARGUMENTS\"`\n\n\
提交成功后，不要自己执行 WebSearch / Read / 通用 Bash，也不要下场做这个任务。\n\
如需安静查看状态，只允许用：\n\
`rccb --project-dir . inbox --instance default --req-id <req_id> --latest --limit 5`\n\n\
如果任务依赖外部事实或资料，请改用 `/rccb-research`。",
            ),
            kind: RuleFileKind::PlainMarkdown,
        });
    }
    if providers.iter().any(|p| p == "claude") && providers.iter().any(|p| p == "gemini") {
        specs.push(RuleFileSpec {
            path: project_dir
                .join(".claude")
                .join("commands")
                .join("rccb-research.md"),
            contents: build_claude_command_markdown(
                "委派调研任务给 gemini，并要求 codex 复核",
                "使用 RCCB 先把调研任务委派给 `gemini`，要求它至少做两轮检索与交叉验证，优先官方/一手来源。\n\
任务内容：$ARGUMENTS\n\n\
在 gemini 返回后，不要直接采纳结论；继续把关键结论、风险点和冲突信息委派给 `codex` 做复核。\n\
整个过程中优先通过 `delegate-researcher` 子代理异步委派，主编排者不要直接下场执行。\n\n\
推荐派单：\n\
`RCCB_ASK_ASYNC_STDOUT=minimal rccb --project-dir . ask --instance default --provider gemini --caller claude --async \"$ARGUMENTS\"`\n\n\
提交成功后，不要自己执行 WebSearch / Read / 通用 Bash，也不要自己完成这项调研。\n\
如需安静查看状态，只允许用：\n\
`rccb --project-dir . inbox --instance default --req-id <req_id> --latest --limit 5`。",
            ),
            kind: RuleFileKind::PlainMarkdown,
        });
    }
    if providers.iter().any(|p| p == "claude") && providers.iter().any(|p| p == "codex") {
        specs.push(RuleFileSpec {
            path: project_dir
                .join(".claude")
                .join("commands")
                .join("rccb-audit.md"),
            contents: build_claude_command_markdown(
                "委派代码审计任务给 codex",
                "使用 RCCB 把代码审计、风险评估、边界条件检查、回归分析和调研复核任务委派给 `codex`。\n\
任务内容：$ARGUMENTS\n\n\
优先通过 `delegate-auditor` 子代理完成派单，主编排者不要直接下场执行。\n\
请直接异步委派，前台只保留最小提交信息，不要自己轮询状态：\n\
`RCCB_ASK_ASYNC_STDOUT=minimal rccb --project-dir . ask --instance default --provider codex --caller claude --async \"$ARGUMENTS\"`\n\n\
提交成功后，不要自己执行 WebSearch / Read / 通用 Bash，也不要自己做审计。\n\
如需安静查看状态，只允许用：\n\
`rccb --project-dir . inbox --instance default --req-id <req_id> --latest --limit 5`",
            ),
            kind: RuleFileKind::PlainMarkdown,
        });
    }
    if providers.iter().any(|p| p == "claude") && providers.iter().any(|p| p == "droid") {
        specs.push(RuleFileSpec {
            path: project_dir
                .join(".claude")
                .join("commands")
                .join("rccb-doc.md"),
            contents: build_claude_command_markdown(
                "委派文档记录任务给 droid",
                "使用 RCCB 把文档整理、纪要、变更说明、操作手册和复盘归档任务委派给 `droid`。\n\
任务内容：$ARGUMENTS\n\n\
优先通过 `delegate-scribe` 子代理完成派单，主编排者不要直接下场执行。\n\
请直接异步委派，前台只保留最小提交信息，不要自己轮询状态：\n\
`RCCB_ASK_ASYNC_STDOUT=minimal rccb --project-dir . ask --instance default --provider droid --caller claude --async \"$ARGUMENTS\"`\n\n\
提交成功后，不要自己执行 WebSearch / Read / 通用 Bash，也不要自己做文档交付。\n\
如需安静查看状态，只允许用：\n\
`rccb --project-dir . inbox --instance default --req-id <req_id> --latest --limit 5`",
            ),
            kind: RuleFileKind::PlainMarkdown,
        });
    }

    if providers.iter().any(|p| p == "droid") {
        specs.push(RuleFileSpec {
            path: project_dir
                .join(".factory")
                .join("commands")
                .join("rccb-code.md"),
            contents: build_factory_command_markdown(
                "委派编码任务给 opencode",
                "当任务需要改代码、修复实现、运行测试或联调时，优先通过 RCCB 把任务委派给 `opencode`。\n\
任务内容：$ARGUMENTS",
            ),
            kind: RuleFileKind::PlainMarkdown,
        });
        specs.push(RuleFileSpec {
            path: project_dir
                .join(".factory")
                .join("commands")
                .join("rccb-research.md"),
            contents: build_factory_command_markdown(
                "先让 gemini 调研，再让 codex 复核",
                "当任务涉及联网调研、网页资料、版本差异或事实核验时，先把任务委派给 `gemini` 做至少两轮调研，再委派给 `codex` 复核关键结论。\n\
任务内容：$ARGUMENTS",
            ),
            kind: RuleFileKind::PlainMarkdown,
        });
        specs.push(RuleFileSpec {
            path: project_dir
                .join(".factory")
                .join("rules")
                .join("rccb-core.md"),
            contents: build_factory_rule_markdown(
                "RCCB 核心协作规则",
                &[
                    "本项目统一通过 `rccb` 完成委派与结果消费，优先使用项目级规则文件和工件文件。",
                    "编排者不直接执行 bash、修改文件或运行测试。",
                    "静默模式下最终结果优先读取 `.rccb/tasks/<instance>/artifacts/<req_id>.reply.md`。",
                    "请求超时时先用 `watch --req-id` 查看真实状态，不要立刻重派。",
                ],
            ),
            kind: RuleFileKind::PlainMarkdown,
        });
        specs.push(RuleFileSpec {
            path: project_dir
                .join(".factory")
                .join("droids")
                .join("researcher.md"),
            contents: build_factory_droid_markdown(
                "researcher",
                "默认调研者，优先承担联网调研、事实核验、资料汇总。",
                &[
                    "至少进行两轮调研：第一轮收集官方/一手来源，第二轮交叉验证关键结论、日期、风险和限制。",
                    "遇到冲突信息时必须明确写出冲突点，不要自行抹平。",
                    "输出要便于后续由 codex 进行复核。",
                ],
            ),
            kind: RuleFileKind::PlainMarkdown,
        });
        specs.push(RuleFileSpec {
            path: project_dir
                .join(".factory")
                .join("droids")
                .join("scribe.md"),
            contents: build_factory_droid_markdown(
                "scribe",
                "默认文档记录者，优先承担文档整理、纪要、操作手册和复盘归档。",
                &[
                    "优先输出结构清晰、可审阅、可追溯的文档。",
                    "如果文档结论依赖外部事实，应提醒编排者先完成 gemini 调研和 codex 复核。",
                    "不要把自己当作编排者或代码审计者。",
                ],
            ),
            kind: RuleFileKind::PlainMarkdown,
        });
    }

    specs
}

fn ensure_managed_markdown_file(path: &Path, managed: &str, mode: BootstrapMode) -> Result<bool> {
    if path.exists() && matches!(mode, BootstrapMode::MissingOnly) {
        return Ok(false);
    }

    let mut user_block = String::new();
    if path.exists() {
        let existing = fs::read_to_string(path)
            .with_context(|| format!("读取规则文件失败：{}", path.display()))?;
        if let Some(block) = extract_between_markers(&existing, RCCB_USER_BEGIN, RCCB_USER_END) {
            user_block = block.trim().to_string();
        }
    }

    let next = format!(
        "{RCCB_MANAGED_BEGIN}\n{managed}\n{RCCB_MANAGED_END}\n\n{RCCB_USER_BEGIN}\n{user_block}\n{RCCB_USER_END}\n"
    );
    write_rule_file(path, &next)?;
    Ok(true)
}

fn ensure_plain_markdown_file(path: &Path, contents: &str, mode: BootstrapMode) -> Result<bool> {
    if path.exists() && matches!(mode, BootstrapMode::MissingOnly) {
        return Ok(false);
    }
    write_rule_file(path, contents)?;
    Ok(true)
}

fn write_rule_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents.as_bytes())
        .with_context(|| format!("写入规则文件失败：{}", path.display()))
}

fn extract_between_markers(text: &str, start: &str, end: &str) -> Option<String> {
    let start_idx = text.find(start)?;
    let tail = &text[start_idx + start.len()..];
    let end_idx = tail.find(end)?;
    Some(tail[..end_idx].trim_matches('\n').to_string())
}

fn contains_provider(providers: &[String], provider: &str) -> bool {
    providers.iter().any(|p| p == provider)
}

fn build_agents_rules_markdown(providers: &[String]) -> String {
    let mut role_lines = Vec::new();
    if contains_provider(providers, "claude") {
        role_lines.push("- `claude`：默认编排者，只负责思考、拆解、分派、验收、汇总。");
    }
    if contains_provider(providers, "opencode") {
        role_lines.push("- `opencode`：默认编码者，优先承担实现、修复、重构、运行测试、联调。");
    }
    if contains_provider(providers, "gemini") {
        role_lines.push("- `gemini`：默认调研者，优先承担联网调研、资料搜集、事实核对、版本确认。");
    }
    if contains_provider(providers, "droid") {
        role_lines.push(
            "- `droid`：默认文档记录者，优先承担文档整理、纪要、变更说明、操作手册、复盘归档。",
        );
    }
    if contains_provider(providers, "codex") {
        role_lines.push("- `codex`：默认代码审计者，优先承担代码审查、风险识别、边界条件检查，以及对调研结论的核验。");
    }
    let mut research_lines = Vec::new();
    if contains_provider(providers, "gemini") {
        research_lines.push("- 只要任务涉及外部事实、时间敏感信息、网页资料、版本差异或供应商能力判断，优先先派给 `gemini`。");
        research_lines.push("- `gemini` 的调研至少做两轮：第一轮收集官方/一手来源，第二轮交叉验证关键结论、日期和风险点。");
    }
    if contains_provider(providers, "codex") && contains_provider(providers, "gemini") {
        research_lines
            .push("- 任何会影响实现、设计决策或最终结论的调研结果，都必须再派给 `codex` 做复核。");
        research_lines
            .push("- `codex` 复核时重点关注：事实冲突、过期信息、落地风险、边界条件、遗漏约束。");
    }
    format!(
        "# RCCB 项目协作规则\n\n\
本仓库以 `rccb` 为统一委派入口。优先使用项目级规则文件和技能，不依赖临时 pane 注入。\n\n\
## 当前 provider 集合\n\
{}\n\n\
## 编排原则\n\
- 默认把第一个 provider 当作编排者，其余 provider 当作执行者。\n\
- 编排者不要自己执行 bash、不要自己改文件、不要自己跑测试。\n\
- 主编排者优先调用对应委派子代理派单，不要直接下场执行执行者任务。\n\
- 所有执行任务统一通过 `rccb --project-dir . ask --instance default --provider <执行者> --caller <编排者> \"<任务>\"` 下发。\n\
- 选择执行者时优先匹配其默认职责；只有确有必要时才跨职责派单。\n\n\
## 调研核验链路\n\
{}\n\n\
## 实时状态与结果\n\
- 静默模式下，最终结果以 `.rccb/tasks/<instance>/artifacts/<req_id>.reply.md` 为准。\n\
- 长任务前台最多确认一次“已委派，等待后台结果”，不要循环播报等待、重复贴命令或频繁自查。\n\
- 派单后不要自己执行 WebSearch / Read / 通用 Bash，也不要自己下场完成该任务。\n\
- 如需安静查看某个请求的最新状态与结果，只允许执行 `rccb --project-dir . inbox --instance default --req-id <req_id> --latest --limit 5`。\n\
- 如果同步 `ask` 超时，不要立刻重派；只允许用 `rccb watch --instance default --req-id <req_id> --with-provider-log --timeout-s 3 --pane-ui` 查看真实状态。\n\
- `watch --follow` 只用于 debug pane 或用户明确要求持续追踪时，不要默认在编排者前台使用。\n\
- 调试日志只应出现在 debug pane，不要把旁路日志刷进 provider 的前台 pane。\n\n\
## 托管与自定义\n\
- 本文件由 RCCB 托管生成，普通启动只补缺失文件，不覆盖现有内容。\n\
- `debug` 模式启动时，RCCB 会刷新托管区块，方便联调规则。\n\
- 请把项目级个性化规则写在下方用户区块中；RCCB 刷新托管区块时会保留用户区块。",
        role_lines.join("\n"),
        if research_lines.is_empty() {
            "- 当前 provider 集合未启用专门调研链路，请按实际启用的执行者调整。".to_string()
        } else {
            research_lines.join("\n")
        }
    )
}

fn build_claude_rules_markdown(providers: &[String]) -> String {
    let mut dispatch_lines = Vec::new();
    if contains_provider(providers, "opencode") {
        dispatch_lines.push("- 实现、改代码、运行测试、修复问题：优先派给 `opencode`");
    }
    if contains_provider(providers, "gemini") {
        dispatch_lines.push("- 调研、搜集资料、核对外部事实：优先派给 `gemini`");
    }
    if contains_provider(providers, "droid") {
        dispatch_lines.push("- 文档、纪要、归档、说明整理：优先派给 `droid`");
    }
    if contains_provider(providers, "codex") {
        dispatch_lines.push("- 代码审计、风险核验、调研复核：优先派给 `codex`");
    }
    let research_rules = if contains_provider(providers, "gemini")
        && contains_provider(providers, "codex")
    {
        "- 涉及外部事实时，先委派 `gemini` 做至少两轮调研与交叉验证。\n- `gemini` 返回后，不要直接采纳；继续委派 `codex` 复核关键结论、日期、风险和边界条件。\n- 没有 `codex` 复核时，不要把调研结果当成最终依据。".to_string()
    } else if contains_provider(providers, "gemini") {
        "- 涉及外部事实时，先委派 `gemini` 做至少两轮调研与交叉验证。\n- 当前未启用 `codex`，采纳调研结论前请额外人工复核关键事实。".to_string()
    } else {
        "- 当前未启用专门调研执行者；若任务依赖外部事实，请谨慎处理并优先补充调研 provider。"
            .to_string()
    };
    format!(
        "# Claude 编排规则\n\n\
你在本项目中的默认角色是编排者。除非用户明确改派，否则不要自己执行 bash、修改文件或运行测试。\n\
所有执行型任务优先通过对应的 Claude 委派子代理派出，不要让主编排者直接下场执行。\n\n\
## 默认派单分工\n\
{}\n\n\
## 调研强约束\n\
{}\n\n\
## 标准命令\n\
```bash\n\
rccb --project-dir . ask --instance default --provider <执行者> --caller claude \"<任务>\"\n\
```\n\n\
## 查看真实状态\n\
- 委派成功后，前台最多确认一次“已委派，等待后台结果”；不要循环播报等待、不要频繁自查。\n\
- 派单后不要自己执行 WebSearch / Read / 通用 Bash，也不要自己下场完成这个任务。\n\
- 如需安静查看最新状态与结果，只允许执行：\n\
```bash\n\
rccb --project-dir . inbox --instance default --req-id <req_id> --latest --limit 5\n\
```\n\
- 若请求超时或你不确定执行者是否仍在运行，再执行：\n\
```bash\n\
rccb --project-dir . watch --instance default --req-id <req_id> --with-provider-log --timeout-s 3 --pane-ui\n\
```\n\
- 静默结果消费以 `.reply.md` 工件为准，但运行态判断优先看 `watch`。\n\
- `watch --follow` 只用于 debug pane 或用户明确要求持续观察时，不要默认在前台使用。\n\n\
## 托管与自定义\n\
- 本文件由 RCCB 托管生成；普通模式不覆盖已有文件。\n\
- `debug` 模式会刷新托管区块，便于联调。\n\
- 自定义规则请写在下方用户区块。",
        dispatch_lines.join("\n"),
        research_rules
    )
}

fn build_gemini_rules_markdown(providers: &[String]) -> String {
    let verify_tip = if contains_provider(providers, "codex") {
        "- 如果用户要基于调研结果做实现或结论，请明确提醒：还需要由 `codex` 做复核。"
    } else {
        "- 当前未启用 `codex` 复核者；输出时请更明确地区分“已确认 / 待确认 / 风险项”。"
    };
    format!(
        "# Gemini 调研规则\n\n\
你在本项目中的默认角色是调研者。优先承担联网调研、事实核对、版本信息确认、资料汇总。\n\n\
## 调研要求\n\
- 对外部事实、版本、发布时间、网页资料、供应商能力判断，至少执行两轮调研。\n\
- 第一轮优先搜集官方或一手来源；第二轮交叉验证关键结论、日期、风险和限制条件。\n\
- 如果遇到冲突信息，要明确写出冲突点，不要自行抹平。\n\
- 输出时尽量给出来源线索、日期、置信度和未确认项，方便后续由 `codex` 复核。\n\n\
## 建议工作流\n\
1. 先把问题拆成若干可验证子问题。\n\
2. 第一轮检索时优先官方文档、源仓库、发行说明、标准文档或一手公告。\n\
3. 第二轮至少换一种检索路径，专门验证第一轮的关键日期、版本、限制条件与风险点。\n\
4. 把“已确认 / 待确认 / 存在冲突”明确分区。\n\
5. 交付前提醒编排者将关键结论交给 `codex` 复核。\n\n\
## 输出建议\n\
- 先给结论摘要\n\
- 再给证据点与日期\n\
- 最后列出风险、冲突与待确认项\n\
- 不要只给单轮搜索结论\n\n\
## 边界\n\
- 除非任务明确要求，否则不要把自己当成最终代码审计者。\n\
- 除非任务明确要求，否则不要承担文档归档者职责。\n\
{}\n\n\
## RCCB 交互\n\
- 你通常通过 RCCB 收到任务，回复内容应尽量结构化，便于编排者继续派单。\n\
- 如需长内容，保持正文清晰，不要重复协议占位文本。",
        verify_tip
    )
}

fn build_skill_frontmatter(name: &str, description: &str) -> String {
    format!(
        "---\n\
name: {name}\n\
description: {description}\n\
---\n\n"
    )
}

fn build_agents_delegate_skill_markdown(providers: &[String]) -> String {
    let mut choices = Vec::new();
    if contains_provider(providers, "opencode") {
        choices.push("- `opencode`：编码、改文件、运行测试、修复实现问题");
    }
    if contains_provider(providers, "gemini") {
        choices.push("- `gemini`：联网调研、资料搜集、事实核对");
    }
    if contains_provider(providers, "droid") {
        choices.push("- `droid`：文档、纪要、复盘、整理记录");
    }
    if contains_provider(providers, "codex") {
        choices.push("- `codex`：代码审计、风险核验、边界检查，以及对调研结果的复核");
    }
    let research_chain = if contains_provider(providers, "gemini")
        && contains_provider(providers, "codex")
    {
        "- 如果任务涉及外部事实或网页资料，先委派 `gemini` 做至少两轮调研。\n- `gemini` 返回后，再委派 `codex` 复核关键结论、日期、风险和遗漏项。".to_string()
    } else if contains_provider(providers, "gemini") {
        "- 如果任务涉及外部事实或网页资料，先委派 `gemini` 做至少两轮调研。".to_string()
    } else {
        "- 当前 provider 集合未启用专门调研链路。".to_string()
    };
    format!(
        "{}# 技能：rccb-delegate\n\n\
## 用途\n\
通过 `rccb` 把执行任务委派给合适的执行者，并在静默模式或超时场景下用 `watch`/`reply.md` 获取真实状态与最终结果。\n\n\
## 选择执行者\n\
{}\n\n\
## 调研链路\n\
{}\n\n\
## 标准命令\n\
```bash\n\
rccb --project-dir . ask --instance default --provider <provider> --caller <caller> \"<task>\"\n\
```\n\n\
## 运行态查看\n\
- 如果 `ask` 超时，不要默认任务失败。\n\
- 默认不要主动执行 `Read file`、`Bash sleep`、`cat .reply.md`、`watch --follow` 等轮询动作。\n\
- 如需安静查看某个请求的最新状态与结果，优先执行：\n\
```bash\n\
rccb --project-dir . inbox --instance default --req-id <req_id> --latest --limit 5\n\
```\n\
- 只有任务超时、异常或用户明确要求实时观察时，再执行：\n\
```bash\n\
rccb --project-dir . watch --instance default --req-id <req_id> --with-provider-log --timeout-s 3 --pane-ui\n\
```\n\n\
## 工件约定\n\
- 请求工件：`.rccb/tasks/<instance>/artifacts/<req_id>.request.md`\n\
- 结果工件：`.rccb/tasks/<instance>/artifacts/<req_id>.reply.md`\n\
- 静默消费最终结果时，优先读取 `reply.md`。",
        build_skill_frontmatter(
            "rccb-delegate",
            "通过 RCCB 委派执行任务，并在静默模式或超时场景下用 watch 与 reply.md 获取真实状态和最终结果。"
        ),
        choices.join("\n"),
        research_chain
    )
}

fn build_agents_audit_skill_markdown() -> String {
    format!(
        "{}# 技能：rccb-audit\n\n\
## 用途\n\
当你在本项目中承担 `codex` 的默认职责时，优先用这套规则完成代码审计、风险分析、边界条件检查和回归评估。\n\n\
## 审计重点\n\
- 行为回归\n\
- 边界条件与异常路径\n\
- 并发/时序问题\n\
- 与 pane、实时状态、静默结果消费相关的协议一致性\n\
- 缺失测试与未覆盖场景\n\n\
## 结论结构\n\
1. 先列最严重的问题\n\
2. 每个问题明确：影响范围、触发条件、为何危险\n\
3. 说明是否需要补测或追加验证\n\
\n\
## 和调研链路的关系\n\
- 如果上游结论来自 `gemini` 调研，请重点复核日期、版本、事实冲突和是否能真正落到当前代码路径。\n\
- 不要只复述调研结论，要给出采纳/不采纳的判断。",
        build_skill_frontmatter(
            "rccb-audit",
            "用于代码审计、风险分析、边界条件检查和对调研结论的复核。"
        )
    )
}

fn build_agents_research_verify_skill_markdown() -> String {
    format!(
        "{}# 技能：rccb-research-verify\n\n\
## 用途\n\
专门用于 `codex` 对 `gemini` 调研结果做第二阶段复核。\n\n\
## 复核步骤\n\
1. 先抽取 `gemini` 给出的关键结论、日期、版本号和风险点。\n\
2. 对最影响决策的结论逐条核验，不要平均用力。\n\
3. 优先检查：是否过期、是否误读、是否缺少适用前提、是否和当前项目上下文不匹配。\n\
4. 输出采纳建议：可直接采纳、需要附条件采纳、不能采纳。\n\n\
## 输出模板\n\
- 已采纳结论\n\
- 有条件采纳结论\n\
- 不建议采纳的结论\n\
- 仍待补证据的点\n\
\n\
## 原则\n\
- 复核不是重复总结，而是做筛错和减风险。\n\
- 如果证据不足，要明确说不足，不要替上游补脑。",
        build_skill_frontmatter(
            "rccb-research-verify",
            "用于对 gemini 的调研结果做第二阶段复核，筛除事实错误、过期信息和落地风险。"
        )
    )
}

fn build_opencode_delegate_skill_markdown(providers: &[String]) -> String {
    let mut mappings = vec!["- `opencode`：编码实现、修复、测试、联调".to_string()];
    if contains_provider(providers, "gemini") {
        mappings.push("- `gemini`：联网调研、资料搜集、事实核对".to_string());
    }
    if contains_provider(providers, "droid") {
        mappings.push("- `droid`：文档、纪要、变更记录、归档".to_string());
    }
    if contains_provider(providers, "codex") {
        mappings.push("- `codex`：代码审计、风险分析、调研复核".to_string());
    }
    let research_rules = if contains_provider(providers, "gemini")
        && contains_provider(providers, "codex")
    {
        "- 涉及外部事实时，先让 `gemini` 做至少两轮调研与交叉验证。\n- `gemini` 返回后，再让 `codex` 复核关键结论、日期、风险和边界条件。".to_string()
    } else if contains_provider(providers, "gemini") {
        "- 涉及外部事实时，先让 `gemini` 做至少两轮调研与交叉验证。".to_string()
    } else {
        "- 当前 provider 集合未启用专门调研执行者。".to_string()
    };
    format!(
        "{}# 技能：rccb-delegate\n\n\
通过 `rccb` 在本项目里委派执行任务，并在静默模式下通过 `watch` 和 `reply.md` 获取真实状态与最终结果。\n\n\
## 默认职责映射\n\
{}\n\n\
## 调研约束\n\
{}\n\n\
## 委派命令\n\
```bash\n\
rccb --project-dir . ask --instance default --provider <provider> --caller <caller> \"<task>\"\n\
```\n\n\
## 超时处理\n\
- 默认不要主动执行 `Read file`、`Bash sleep`、`cat .reply.md`、`watch --follow` 等轮询动作。\n\
```bash\n\
rccb --project-dir . inbox --instance default --req-id <req_id> --latest --limit 5\n\
```\n\
- 只有任务超时、异常或用户明确要求实时观察时，再执行：\n\
```bash\n\
rccb --project-dir . watch --instance default --req-id <req_id> --with-provider-log --timeout-s 3 --pane-ui\n\
```\n",
        build_skill_frontmatter(
            "rccb-delegate",
            "通过 RCCB 委派执行任务，并在静默模式下用 watch 与 reply.md 跟踪真实状态和最终结果。"
        ),
        mappings.join("\n"),
        research_rules
    )
}

fn build_factory_delegate_skill_markdown(providers: &[String]) -> String {
    let mut choices = Vec::new();
    if contains_provider(providers, "opencode") {
        choices.push("- `opencode`：编码与测试");
    }
    if contains_provider(providers, "gemini") {
        choices.push("- `gemini`：调研与事实核验");
    }
    choices.push("- `droid`：文档与记录");
    if contains_provider(providers, "codex") {
        choices.push("- `codex`：代码审计与调研复核");
    }
    let research_chain = if contains_provider(providers, "gemini")
        && contains_provider(providers, "codex")
    {
        "1. 先派 `gemini` 做至少两轮调研。\n2. 再派 `codex` 复核关键结论、日期、风险和边界条件。\n3. 没有经过 `codex` 复核时，不要把调研结论当最终依据。".to_string()
    } else if contains_provider(providers, "gemini") {
        "1. 先派 `gemini` 做至少两轮调研。\n2. 当前未启用 `codex`，采纳前请人工复核关键事实。"
            .to_string()
    } else {
        "1. 当前 provider 集合未启用专门调研链路，请按需补充。".to_string()
    };
    format!(
        "{}# RCCB 委派技能\n\n\
## 选择执行者\n\
{}\n\n\
## 调研链路\n\
{}\n\n\
## 标准命令\n\
```bash\n\
rccb --project-dir . ask --instance default --provider <provider> --caller <caller> \"<task>\"\n\
```\n\n\
## 状态查看\n\
- 默认不要主动执行 `Read file`、`Bash sleep`、`cat .reply.md`、`watch --follow` 等轮询动作。\n\
```bash\n\
rccb --project-dir . inbox --instance default --req-id <req_id> --latest --limit 5\n\
```\n\
- 只有任务超时、异常或用户明确要求实时观察时，再执行：\n\
```bash\n\
rccb --project-dir . watch --instance default --req-id <req_id> --with-provider-log --timeout-s 3 --pane-ui\n\
```\n\n\
## 工件驱动\n\
- 最终结果优先来自 `.rccb/tasks/<instance>/artifacts/<req_id>.reply.md`\n\
- 超时后先看 `watch`，不要立刻重复派单。",
        build_skill_frontmatter(
            "rccb-delegate",
            "通过 RCCB 委派执行任务，并在静默模式下用 watch 与 reply.md 跟踪真实状态和最终结果。"
        ),
        choices.join("\n"),
        research_chain
    )
}

fn build_claude_command_markdown(description: &str, body: &str) -> String {
    format!(
        "---\n\
description: {description}\n\
argument-hint: [任务内容]\n\
---\n\n\
{body}\n"
    )
}

fn localized_agent_title(name: &str) -> &str {
    match name {
        "orchestrator" => "编排者",
        "reviewer" => "复核者",
        "coder" => "编码者",
        "auditor" => "审计者",
        "researcher" => "调研者",
        "scribe" => "记录者",
        _ => name,
    }
}

fn build_claude_agent_markdown(name: &str, summary: &str, bullets: &[&str]) -> String {
    let details = bullets
        .iter()
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "---\n\
name: {name}\n\
description: {summary}\n\
---\n\n\
# {}\n\n\
{summary}\n\n\
{details}\n",
        localized_agent_title(name)
    )
}

fn build_opencode_command_markdown(description: &str, body: &str) -> String {
    format!(
        "---\n\
description: {description}\n\
---\n\n\
{body}\n"
    )
}

fn build_opencode_agent_markdown(name: &str, summary: &str, bullets: &[&str]) -> String {
    let details = bullets
        .iter()
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "---\n\
name: {name}\n\
description: {summary}\n\
---\n\n\
# {}\n\n\
{summary}\n\n\
{details}\n",
        localized_agent_title(name)
    )
}

fn build_factory_command_markdown(description: &str, body: &str) -> String {
    format!(
        "---\n\
description: {description}\n\
---\n\n\
{body}\n\n\
标准命令：\n\
`rccb --project-dir . ask --instance default --provider <执行者> --caller <编排者> \"<任务>\"`\n"
    )
}

fn build_factory_rule_markdown(title: &str, bullets: &[&str]) -> String {
    let body = bullets
        .iter()
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("# {title}\n\n{body}\n")
}

fn build_factory_droid_markdown(name: &str, summary: &str, bullets: &[&str]) -> String {
    let details = bullets
        .iter()
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "---\n\
name: {name}\n\
description: {summary}\n\
---\n\n\
# {}\n\n\
{summary}\n\n\
{details}\n",
        localized_agent_title(name)
    )
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
    validate_provider_prerequisites(project_dir, &normalized)?;
    let effective_debug = resolve_start_debug(debug.then_some(true).or_else(env_debug_override));
    let _ = ensure_project_bootstrap(
        project_dir,
        if effective_debug {
            BootstrapMode::RefreshGenerated
        } else {
            BootstrapMode::MissingOnly
        },
        &normalized,
    )?;
    let _ = refresh_legacy_provider_wrappers(project_dir, &normalized)?;

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

pub fn cmd_shortcut_restore(project_dir: &Path) -> Result<()> {
    let providers = resolve_shortcut_restore_providers(project_dir)?;
    launch_shortcut_instance(project_dir, &providers)
}

pub fn cmd_external_provider_launch(project_dir: &Path, raw: Vec<String>) -> Result<()> {
    if raw.is_empty() {
        return cmd_shortcut_restore(project_dir);
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
    launch_shortcut_instance(project_dir, &normalized)
}

fn launch_shortcut_instance(project_dir: &Path, providers: &[String]) -> Result<()> {
    validate_provider_prerequisites(project_dir, providers)?;
    let effective_debug = resolve_start_debug(env_debug_override());
    let _ = ensure_project_bootstrap(
        project_dir,
        if effective_debug {
            BootstrapMode::RefreshGenerated
        } else {
            BootstrapMode::MissingOnly
        },
        providers,
    )?;
    let _ = refresh_legacy_provider_wrappers(project_dir, providers)?;
    restart_default_daemon_for_shortcut(project_dir)?;
    ensure_default_daemon_running(project_dir, providers, effective_debug)?;

    if launch_provider_clis(project_dir, providers, effective_debug)? {
        return Ok(());
    }

    println!(
        "daemon 已就绪：project={} instance=default providers={}",
        project_dir.display(),
        providers.join(",")
    );
    println!("未检测到终端后端（tmux/wezterm），未自动拉起 provider CLI pane。");
    println!(
        "你仍可通过以下方式发起请求：{} ask --instance default --provider {} --caller {} \"...\"",
        project_rccb_command(project_dir),
        providers
            .first()
            .cloned()
            .unwrap_or_else(|| "codex".to_string()),
        providers
            .first()
            .cloned()
            .unwrap_or_else(|| "manual".to_string())
    );
    Ok(())
}

fn resolve_shortcut_restore_providers(project_dir: &Path) -> Result<Vec<String>> {
    if let Some(v) = shortcut_providers_from_launcher_meta(project_dir, SHORTCUT_INSTANCE) {
        return Ok(v);
    }
    if let Some(v) = shortcut_providers_from_state(project_dir, SHORTCUT_INSTANCE) {
        return Ok(v);
    }
    let installed = shortcut_installed_default_providers(project_dir);
    if installed.is_empty() {
        bail!("未找到可恢复的 provider，也未检测到可用 CLI；请显式执行 `rccb claude ...` 或配置 provider 启动命令");
    }
    Ok(installed)
}

fn shortcut_providers_from_launcher_meta(
    project_dir: &Path,
    instance: &str,
) -> Option<Vec<String>> {
    let meta = load_launcher_meta(project_dir, instance)?;
    filter_launchable_providers(
        project_dir,
        &meta
            .providers
            .into_iter()
            .map(|entry| entry.provider)
            .collect::<Vec<_>>(),
    )
}

fn shortcut_providers_from_state(project_dir: &Path, instance: &str) -> Option<Vec<String>> {
    let path = state_path(project_dir, instance);
    let state = load_state(&path).ok()?;
    let mut providers = state.providers;
    if providers.is_empty() {
        if let Some(orchestrator) = state.orchestrator.filter(|v| !v.trim().is_empty()) {
            providers.push(orchestrator);
        }
        providers.extend(state.executors.into_iter().filter(|v| !v.trim().is_empty()));
    }
    filter_launchable_providers(project_dir, &providers)
}

fn shortcut_installed_default_providers(project_dir: &Path) -> Vec<String> {
    SHORTCUT_DEFAULT_PROVIDERS
        .iter()
        .filter(|provider| provider_cli_is_available(project_dir, provider))
        .map(|provider| (*provider).to_string())
        .collect()
}

fn filter_launchable_providers(project_dir: &Path, providers: &[String]) -> Option<Vec<String>> {
    let filtered = providers
        .iter()
        .filter(|provider| provider_cli_is_available(project_dir, provider))
        .cloned()
        .collect::<Vec<_>>();
    if filtered.is_empty() {
        None
    } else {
        Some(filtered)
    }
}

fn validate_provider_prerequisites(project_dir: &Path, providers: &[String]) -> Result<()> {
    let mut missing = Vec::new();
    for provider in providers {
        if provider_cli_is_available(project_dir, provider) {
            continue;
        }
        let key = format!("RCCB_{}_START_CMD", provider.trim().to_ascii_uppercase());
        missing.push(format!(
            "- {}: 未检测到可用 CLI；请安装 `{}`，或设置环境变量 `{}`",
            provider,
            provider_cli_command(provider).unwrap_or(provider),
            key
        ));
    }

    if missing.is_empty() {
        return Ok(());
    }

    bail!(
        "启动前检查失败：基础环境不满足，已取消启动。\n{}\n请先补齐上述 provider CLI，再重新执行。",
        missing.join("\n")
    )
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
        resolve_start_debug(env_debug_override()),
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
        resolve_start_debug(env_debug_override()),
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
    let has_residual_runtime = path.exists()
        || launcher_meta_path(project_dir, instance).exists()
        || session_instance_dir(project_dir, instance).exists()
        || tmp_instance_dir(project_dir, instance).exists();
    if path.exists() {
        let was_running = is_daemon_ready(project_dir, instance);
        if was_running {
            println!("检测到旧的 default 实例仍在运行，正在重启以应用最新规则...");
            cmd_stop(project_dir, instance)?;
        }
    }
    if has_residual_runtime {
        let _ = cleanup_inflight_tasks(project_dir, instance);
        cleanup_instance_runtime(project_dir, instance)?;
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

fn load_launcher_meta(project_dir: &Path, instance: &str) -> Option<LauncherMeta> {
    let meta_path = launcher_meta_path(project_dir, instance);
    let raw = fs::read_to_string(meta_path).ok()?;
    serde_json::from_str(&raw).ok()
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
            maybe_prime_orchestrator_pane(
                project_dir,
                &backend,
                anchor_pane,
                &orchestrator,
                &providers[1..],
            );
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
            maybe_prime_orchestrator_pane(
                project_dir,
                &backend,
                anchor_pane,
                &orchestrator,
                &providers[1..],
            );
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
        if !is_ignorable_pane_command_error(&err) {
            eprintln!("警告：无法聚焦编排者 pane={} err={}", pane, err);
        }
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
    project_dir: &Path,
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
    let prompt = orchestrator_guardrail_prompt(project_dir, orchestrator, executors);
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
        feed_file: None,
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

fn orchestrator_guardrail_prompt(
    project_dir: &Path,
    orchestrator: &str,
    executors: &[String],
) -> String {
    let executor_list = if executors.is_empty() {
        "-".to_string()
    } else {
        executors.join(", ")
    };
    render_project_bootstrap_content(
        project_dir,
        &format!(
        "RCCB 编排模式已启用。\n\n当前编排者：{orchestrator}\n可用执行者：{executor_list}\n\n只做：规划、拆解、委派、验收、汇总。\n不要自己执行 bash、修改文件或运行测试。\n\n默认分工：opencode=编码，gemini=调研，droid=文档，codex=审计。\n调研规则：先 gemini 至少两轮调研，再 codex 复核关键结论。\n\n委派格式：\n`rccb --project-dir . ask --instance default --provider <执行者> --caller {orchestrator} \"<任务>\"`\n\n结果默认走后台 inbox 和 `.reply.md`，不会刷屏。\n前台最多确认一次“已委派，等待后台结果”；不要循环 `sleep`、`cat .reply.md`、`watch --follow` 自己轮询。\n如需安静查看最新状态，优先用 `rccb --project-dir . inbox --instance default --req-id <req_id> --latest --limit 5`。\n若 ask 超时，再用不跟随的一次性 `watch --req-id` 看真实状态，不要立刻重派。\n详细规则见 `AGENTS.md` 与 `CLAUDE.md`。"
        ),
    )
}

fn run_orchestrator_foreground(project_dir: &Path, instance: &str, provider: &str) -> Result<i32> {
    let cmd = provider_start_cmd(project_dir, instance, provider);
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
                let _ = run_simple_quiet_if_missing_pane("tmux", &["kill-pane", "-t", pane]);
            }
            LaunchBackend::Wezterm { bin, .. } => {
                let _ =
                    run_simple_quiet_if_missing_pane(bin, &["cli", "kill-pane", "--pane-id", pane]);
            }
        }
    }

    let _ = cmd_stop(project_dir, SHORTCUT_INSTANCE);
    let _ = cleanup_inflight_tasks(project_dir, SHORTCUT_INSTANCE);
    let _ = cleanup_instance_runtime(project_dir, SHORTCUT_INSTANCE);
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
    if matches!(backend, LaunchBackend::Tmux { .. }) {
        fs::create_dir_all(launcher_feed_dir(project_dir, instance))?;
    }

    let orchestrator = providers
        .first()
        .cloned()
        .unwrap_or_else(|| "orchestrator".to_string());
    let mut entries = Vec::new();
    for provider in providers {
        let feed_file = if matches!(backend, LaunchBackend::Tmux { .. }) {
            let feed_path = launcher_feed_path(project_dir, instance, provider);
            fs::write(&feed_path, b"")?;
            if let Some(pane_id) = provider_panes.get(provider) {
                attach_tmux_feed_pipe(project_dir, instance, provider, pane_id)?;
            }
            feed_path.display().to_string()
        } else {
            String::new()
        };
        entries.push(LauncherProviderMeta {
            provider: provider.clone(),
            role: if provider == &orchestrator {
                "orchestrator".to_string()
            } else {
                "executor".to_string()
            },
            feed_file,
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

fn attach_tmux_feed_pipe(
    project_dir: &Path,
    instance: &str,
    provider: &str,
    pane_id: &str,
) -> Result<()> {
    let exe = env::current_exe().context("获取当前 rccb 可执行文件失败")?;
    let cmd = format!(
        "{} --project-dir {} pane-feed --instance {} --provider {}",
        shell_quote(&exe.display().to_string()),
        shell_quote(&project_dir.display().to_string()),
        shell_quote(instance),
        shell_quote(provider),
    );
    run_simple("tmux", &["pipe-pane", "-O", "-t", pane_id.trim(), &cmd])
}

fn provider_start_cmd(project_dir: &Path, instance: &str, provider: &str) -> String {
    let raw = provider_raw_start_cmd(project_dir, provider);
    let role = launcher_provider_role(project_dir, instance, provider);
    let specialty = default_provider_specialty(provider, role.as_deref());
    let agent = default_provider_agent(provider, role.as_deref());

    let mut prefixes = vec![format!(
        "RCCB_PROJECT_DIR={}",
        shell_quote(&project_dir.display().to_string())
    )];
    if let Some(role) = role.as_deref().filter(|v| !v.trim().is_empty()) {
        prefixes.push(format!("RCCB_PROVIDER_ROLE={}", shell_quote(role)));
    }
    if let Some(specialty) = specialty.filter(|v| !v.trim().is_empty()) {
        prefixes.push(format!(
            "RCCB_PROVIDER_SPECIALTY={}",
            shell_quote(specialty)
        ));
    }
    if let Some(agent) = agent.filter(|v| !v.trim().is_empty()) {
        prefixes.push(format!("RCCB_PROVIDER_AGENT={}", shell_quote(agent)));
    }

    format!(
        "cd {} && {} {}",
        shell_quote(&project_dir.display().to_string()),
        prefixes.join(" "),
        raw
    )
}

fn provider_raw_start_cmd(project_dir: &Path, provider: &str) -> String {
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

    let project_wrapper = rccb_dir(project_dir)
        .join("bin")
        .join(provider.trim().to_ascii_lowercase());
    if is_executable_file(&project_wrapper) {
        return shell_quote(&project_wrapper.display().to_string());
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

fn launcher_provider_role(project_dir: &Path, instance: &str, provider: &str) -> Option<String> {
    let meta_path = launcher_meta_path(project_dir, instance);
    let raw = fs::read_to_string(meta_path).ok()?;
    let meta: LauncherMeta = serde_json::from_str(&raw).ok()?;
    meta.providers
        .into_iter()
        .find(|entry| entry.provider.eq_ignore_ascii_case(provider))
        .map(|entry| entry.role)
}

fn default_provider_specialty(provider: &str, role: Option<&str>) -> Option<&'static str> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "claude" => Some(if !matches!(role, Some("executor")) {
            "编排者"
        } else {
            "复核者"
        }),
        "opencode" => Some("编码者"),
        "gemini" => Some("调研者"),
        "droid" => Some("文档记录者"),
        "codex" => Some("代码审计者"),
        _ => None,
    }
}

fn default_provider_agent(provider: &str, role: Option<&str>) -> Option<&'static str> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "claude" if !matches!(role, Some("executor")) => Some("orchestrator"),
        "claude" => Some("reviewer"),
        "opencode" => Some("coder"),
        _ => None,
    }
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

fn provider_cli_is_available(_project_dir: &Path, provider: &str) -> bool {
    let key = format!("RCCB_{}_START_CMD", provider.trim().to_ascii_uppercase());
    if env::var(&key)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    if env_bool("RCCB_USE_BRIDGE_PROVIDER_LAUNCH", false) && resolve_bridge_launch_cmd().is_some() {
        return true;
    }
    provider_cli_command(provider)
        .map(command_exists_on_path)
        .unwrap_or(false)
}

fn provider_cli_command(provider: &str) -> Option<&'static str> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        "gemini" => Some("gemini"),
        "opencode" => Some("opencode"),
        "droid" => Some("droid"),
        _ => None,
    }
}

fn command_exists_on_path(cmd: &str) -> bool {
    let candidate = Path::new(cmd);
    if candidate.components().count() > 1 {
        return is_executable_file(candidate);
    }

    env::var_os("PATH")
        .map(|paths| {
            env::split_paths(&paths)
                .map(|dir| dir.join(cmd))
                .any(|path| is_executable_file(&path))
        })
        .unwrap_or(false)
}

fn is_executable_file(path: &Path) -> bool {
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
    let from_env = env::var("SHELL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());

    if let Some(shell) = from_env.filter(|v| Path::new(v).is_absolute() && Path::new(v).exists()) {
        return shell;
    }
    if Path::new("/bin/bash").exists() {
        return "/bin/bash".to_string();
    }
    if Path::new("/bin/sh").exists() {
        return "/bin/sh".to_string();
    }
    "sh".to_string()
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
    let out = ProcessCommand::new(bin)
        .args(args)
        .output()
        .with_context(|| format!("run command failed: {} {:?}", bin, args))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    bail!(
        "command failed: {} {:?} status={} stdout=`{}` stderr=`{}`",
        bin,
        args,
        out.status,
        stdout,
        stderr
    );
}

fn run_simple_quiet_if_missing_pane(bin: &str, args: &[&str]) -> Result<()> {
    match run_simple(bin, args) {
        Ok(()) => Ok(()),
        Err(err) if is_ignorable_pane_command_error(&err) => Ok(()),
        Err(err) => Err(err),
    }
}

fn is_ignorable_pane_command_error(err: &anyhow::Error) -> bool {
    let text = err.to_string().to_ascii_lowercase();
    [
        "can't find pane",
        "pane not found",
        "no such pane",
        "unknown pane",
        "target window not found",
        "can't find window",
        "no such window",
    ]
    .iter()
    .any(|needle| text.contains(needle))
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
    #[serde(skip_serializing_if = "Option::is_none")]
    request_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_file: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct InboxEntryView {
    instance: String,
    orchestrator: String,
    kind: String,
    req_id: Option<String>,
    executor: Option<String>,
    caller: Option<String>,
    status: Option<String>,
    exit_code: Option<i32>,
    ts_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_file: Option<String>,
}

pub fn cmd_inbox(
    project_dir: &Path,
    instance: &str,
    orchestrator: Option<&str>,
    req_id: Option<&str>,
    kind: Option<&str>,
    latest: bool,
    limit: usize,
    as_json: bool,
) -> Result<()> {
    ensure_project_layout(project_dir)?;
    let orchestrator = resolve_inbox_orchestrator(project_dir, instance, orchestrator)?;
    let mut items = load_orchestrator_inbox_entries(project_dir, instance, &orchestrator)?;

    if let Some(req_id) = req_id.map(str::trim).filter(|v| !v.is_empty()) {
        items.retain(|x| x.req_id.as_deref() == Some(req_id));
    }
    if let Some(kind) = kind.map(str::trim).filter(|v| !v.is_empty()) {
        items.retain(|x| x.kind.eq_ignore_ascii_case(kind));
    }
    if latest {
        items = collapse_inbox_entries_latest(items);
    }

    items.reverse();
    if limit > 0 && items.len() > limit {
        items.truncate(limit);
    }

    if as_json {
        let val = json!({
            "project": project_dir.display().to_string(),
            "instance": instance,
            "orchestrator": orchestrator,
            "entries": items,
        });
        println!("{}", serde_json::to_string_pretty(&val)?);
        return Ok(());
    }

    if items.is_empty() {
        println!(
            "未找到 inbox 条目：instance={} orchestrator={}",
            instance, orchestrator
        );
        return Ok(());
    }

    println!(
        "编排者 inbox：instance={} orchestrator={} 条目数={}",
        instance,
        orchestrator,
        items.len()
    );
    for item in items {
        println!(
            "- kind={} req_id={} executor={} status={} exit={} ts={}",
            item.kind,
            item.req_id.unwrap_or_else(|| "-".to_string()),
            item.executor.unwrap_or_else(|| "-".to_string()),
            item.status.unwrap_or_else(|| "-".to_string()),
            item.exit_code
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string()),
            item.ts_unix
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string()),
        );
        if let Some(message) = item
            .message
            .as_deref()
            .map(|v| compact_inbox_text(v, 160))
            .filter(|v| !v.is_empty())
        {
            println!("  message={}", message);
        }
        if let Some(reply) = item
            .reply
            .as_deref()
            .map(|v| compact_inbox_text(v, 200))
            .filter(|v| !v.is_empty())
        {
            println!("  reply={}", reply);
        }
        if let Some(path) = item.reply_file.as_deref().filter(|v| !v.trim().is_empty()) {
            println!("  reply_file={}", path);
        }
    }
    Ok(())
}

fn collapse_inbox_entries_latest(items: Vec<InboxEntryView>) -> Vec<InboxEntryView> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items.into_iter().rev() {
        let key = inbox_entry_latest_key(&item);
        if seen.insert(key) {
            out.push(item);
        }
    }
    out.reverse();
    out
}

fn inbox_entry_latest_key(item: &InboxEntryView) -> String {
    let req_id = item.req_id.as_deref().unwrap_or("-");
    let executor = item.executor.as_deref().unwrap_or("-");
    let kind_group = if item.kind.eq_ignore_ascii_case("result") {
        "result"
    } else {
        "status"
    };
    format!("{req_id}\t{executor}\t{kind_group}")
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

fn resolve_inbox_orchestrator(
    project_dir: &Path,
    instance: &str,
    explicit: Option<&str>,
) -> Result<String> {
    if let Some(v) = explicit.map(str::trim).filter(|v| !v.is_empty()) {
        return Ok(v.to_string());
    }
    let path = state_path(project_dir, instance);
    if path.exists() {
        let state = load_state(&path)?;
        if let Some(orchestrator) = state.orchestrator.filter(|v| !v.trim().is_empty()) {
            return Ok(orchestrator);
        }
    }
    bail!("缺少 --orchestrator，且无法从实例状态推断编排者");
}

fn load_orchestrator_inbox_entries(
    project_dir: &Path,
    instance: &str,
    orchestrator: &str,
) -> Result<Vec<InboxEntryView>> {
    let path = tmp_instance_dir(project_dir, instance)
        .join("orchestrator")
        .join(format!("{}.jsonl", sanitize_filename(orchestrator)));
    if !path.exists() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    let file =
        File::open(&path).with_context(|| format!("打开编排者 inbox 失败：{}", path.display()))?;
    let reader = io::BufReader::new(file);
    for line in reader.lines() {
        let line = match line {
            Ok(v) => v,
            Err(_) => continue,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        out.push(InboxEntryView {
            instance: instance.to_string(),
            orchestrator: orchestrator.to_string(),
            kind: v
                .get("kind")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown")
                .to_string(),
            req_id: v
                .get("req_id")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
            executor: v
                .get("executor")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
            caller: v
                .get("caller")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
            status: v
                .get("status")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
            exit_code: v
                .get("exit_code")
                .and_then(|x| x.as_i64())
                .map(|x| x as i32),
            ts_unix: v.get("ts_unix").and_then(|x| x.as_u64()),
            message: v
                .get("message")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
            reply: v
                .get("reply")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
            reply_file: v
                .get("reply_file")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
        });
    }
    Ok(out)
}

fn compact_inbox_text(raw: &str, max_chars: usize) -> String {
    let single = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let compact = single.trim();
    if compact.chars().count() <= max_chars {
        return compact.to_string();
    }
    let mut out = String::new();
    for ch in compact.chars() {
        if out.chars().count() >= max_chars {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
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
        let request_file = artifact_path_from_value(&v, "request_file");
        let reply_file = artifact_path_from_value(&v, "reply_file");
        let reply = resolve_task_reply(&v, reply_file.as_deref());

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
            reply,
            request_file,
            reply_file,
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
        let request_file = artifact_path_from_value(&v, "request_file");
        let reply_file = artifact_path_from_value(&v, "reply_file");
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
            reply: resolve_task_reply(&v, reply_file.as_deref()),
            request_file,
            reply_file,
        }));
    }

    for task in load_tasks_in_instance(project_dir, instance)? {
        if task.req_id.as_deref() == Some(req_id) {
            return Ok(Some(task));
        }
    }

    Ok(None)
}

fn artifact_path_from_value(v: &Value, key: &str) -> Option<String> {
    v.get("artifacts")
        .and_then(|x| x.get(key))
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
}

fn resolve_task_reply(v: &Value, reply_file: Option<&str>) -> Option<String> {
    if let Some(path) = reply_file.map(str::trim).filter(|p| !p.is_empty()) {
        if let Ok(reply) = fs::read_to_string(path) {
            return Some(reply);
        }
    }
    v.get("reply")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
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
        cleanup_instance_runtime(project_dir, instance)?;
        println!(
            "instance={} 未发现运行中状态，已清理残留运行态文件",
            instance
        );
        return Ok(());
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
    if stopped {
        cleanup_instance_runtime(project_dir, instance)?;
    }
    Ok(())
}

fn cleanup_instance_runtime(project_dir: &Path, instance: &str) -> Result<()> {
    cleanup_launcher_bindings(project_dir, instance);

    let state_file = state_path(project_dir, instance);
    if state_file.exists() {
        let state = load_state(&state_file)?;
        if is_process_alive(state.pid) && state.status != "stopped" {
            bail!(
                "实例仍在运行，暂不清理运行态文件：instance={} pid={}",
                instance,
                state.pid
            );
        }
    }

    remove_path_if_exists(&state_file)?;
    remove_path_if_exists(&lock_path(project_dir, instance))?;
    remove_path_if_exists(&session_instance_dir(project_dir, instance))?;
    remove_path_if_exists(&tmp_instance_dir(project_dir, instance))?;
    Ok(())
}

fn cleanup_launcher_bindings(project_dir: &Path, instance: &str) {
    let Some(meta) = load_launcher_meta(project_dir, instance) else {
        return;
    };

    if meta.backend != "tmux" {
        return;
    }

    for provider in meta.providers {
        let Some(pane_id) = provider
            .pane_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        let _ = run_simple_quiet_if_missing_pane("tmux", &["pipe-pane", "-t", pane_id]);
        let _ = run_simple_quiet_if_missing_pane("tmux", &["select-pane", "-t", pane_id, "-T", ""]);
    }
}

fn remove_path_if_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let meta = fs::metadata(path)?;
    if meta.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
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
        .clone()
        .ok_or_else(|| anyhow!("missing daemon_host in state"))?;
    let port = state
        .daemon_port
        .ok_or_else(|| anyhow!("missing daemon_port in state"))?;
    let token = state
        .daemon_token
        .clone()
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
        .clone()
        .ok_or_else(|| anyhow!("missing daemon_host in state"))?;
    let port = state
        .daemon_port
        .ok_or_else(|| anyhow!("missing daemon_port in state"))?;
    let token = state
        .daemon_token
        .clone()
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
        .clone()
        .ok_or_else(|| anyhow!("missing daemon_host in state"))?;
    let port = state
        .daemon_port
        .ok_or_else(|| anyhow!("missing daemon_port in state"))?;
    let token = state
        .daemon_token
        .clone()
        .ok_or_else(|| anyhow!("missing daemon_token in state"))?;

    let message = if message_parts.is_empty() {
        read_stdin_all()?.trim().to_string()
    } else {
        message_parts.join(" ")
    };

    if message.trim().is_empty() {
        bail!("message is empty");
    }

    enforce_orchestrator_dispatch_guard(project_dir, instance, &state, &provider, caller)?;

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
        "instance_id": instance,
    });

    if stream {
        return cmd_ask_stream(
            project_dir,
            instance,
            &host,
            port,
            req,
            timeout_s.max(1.0) + 10.0,
        );
    }

    let resp = send_wire_message(&host, port, req, timeout_s.max(1.0) + 5.0)?;
    let parsed: AskResponse =
        serde_json::from_value(resp).context("invalid ask.response payload")?;
    let parsed_reply = resolve_response_reply(
        project_dir,
        instance,
        parsed.req_id.as_deref(),
        parsed.meta.as_ref(),
        &parsed.reply,
    )?;

    if parsed.exit_code == 0 {
        if async_submit {
            let req_id_print = parsed.req_id.unwrap_or_else(|| "-".to_string());
            let provider_print = parsed.provider.unwrap_or_else(|| provider.to_string());
            if req_id_print != "-" && is_orchestrator_executor_call(&state, &provider_print, caller)
            {
                if let Some(orchestrator) = state.orchestrator.as_deref() {
                    let _ = mark_inflight(
                        project_dir,
                        instance,
                        orchestrator,
                        &provider_print,
                        &req_id_print,
                        "submitted",
                    );
                }
            }
            print_async_submit_notice(
                project_dir,
                instance,
                &provider_print,
                &req_id_print,
                "submitted",
            );
            return Ok(());
        }
        if should_suppress_sync_reply_for_orchestrator(&state, &provider, caller) {
            return Ok(());
        }
        if !parsed_reply.is_empty() {
            println!("{}", parsed_reply);
        }
        return Ok(());
    }

    if let Some(req_id) = parsed.req_id.as_deref() {
        if should_degrade_timeout_to_pending(
            project_dir,
            instance,
            &state,
            &provider,
            caller,
            parsed.exit_code,
            parsed.meta.as_ref(),
            req_id,
        )? {
            let provider_print = parsed.provider.unwrap_or_else(|| provider.to_string());
            if let Some(orchestrator) = state.orchestrator.as_deref() {
                let _ = mark_inflight(
                    project_dir,
                    instance,
                    orchestrator,
                    &provider_print,
                    req_id,
                    "running",
                );
            }
            print_async_submit_notice(project_dir, instance, &provider_print, req_id, "running");
            return Ok(());
        }
    }

    bail!(
        "ask failed: exit_code={} reply={} req_id={}",
        parsed.exit_code,
        parsed_reply,
        parsed.req_id.unwrap_or_else(|| "-".to_string())
    )
}

fn enforce_orchestrator_dispatch_guard(
    project_dir: &Path,
    instance: &str,
    state: &InstanceState,
    provider: &str,
    caller: &str,
) -> Result<()> {
    if !env_bool("RCCB_ORCHESTRATOR_WAIT_GUARD", true) {
        return Ok(());
    }
    if env_bool("RCCB_ORCHESTRATOR_ALLOW_PARALLEL", false) {
        return Ok(());
    }
    if !env::var("RCCB_PROVIDER_ROLE")
        .ok()
        .map(|v| v.trim().eq_ignore_ascii_case("orchestrator"))
        .unwrap_or(false)
    {
        return Ok(());
    }
    let current_agent = env::var("RCCB_PROVIDER_AGENT").ok().unwrap_or_default();
    if !current_agent.trim().is_empty()
        && !current_agent.trim().eq_ignore_ascii_case("orchestrator")
    {
        return Ok(());
    }
    if !is_orchestrator_executor_call(state, provider, caller) {
        return Ok(());
    }

    let Some(orchestrator) = state
        .orchestrator
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    else {
        return Ok(());
    };

    let mut inflight = load_inflight(project_dir, instance, orchestrator)?;
    inflight.retain(|entry| {
        let rid = entry.req_id.trim();
        if rid.is_empty() {
            return false;
        }
        match load_task_by_req_id(project_dir, instance, rid) {
            Ok(Some(task)) if is_terminal_task_status(&task.status) => {
                let _ = clear_inflight(project_dir, instance, orchestrator, rid);
                false
            }
            Ok(Some(_)) => true,
            Ok(None) => true,
            Err(_) => true,
        }
    });

    if inflight.is_empty() {
        return Ok(());
    }

    let summary = inflight
        .iter()
        .map(|entry| format!("{}({}->{})", entry.req_id, entry.executor, entry.status))
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "主编排者等待态已生效，当前已有未完成任务：{}。在收到最终结果前，主编排者允许使用 `rccb inbox/watch` 查询状态，但不允许继续直接派发新的执行任务，也不要自己下场执行。若需要并行派单，请改用 Claude 的 `delegate-*` 子代理；如确需临时放开，也可显式设置 `RCCB_ORCHESTRATOR_ALLOW_PARALLEL=1`。",
        summary
    );
}

fn should_suppress_sync_reply_for_orchestrator(
    state: &InstanceState,
    provider: &str,
    caller: &str,
) -> bool {
    if env_bool("RCCB_ORCHESTRATOR_SYNC_STDOUT_RESULT", false) {
        return false;
    }
    if !env_bool("RCCB_ORCHESTRATOR_RESULT_CALLBACK", false) {
        return false;
    }
    is_orchestrator_executor_call(state, provider, caller)
}

fn should_degrade_timeout_to_pending(
    project_dir: &Path,
    instance: &str,
    state: &InstanceState,
    provider: &str,
    caller: &str,
    exit_code: i32,
    meta: Option<&Value>,
    req_id: &str,
) -> Result<bool> {
    if exit_code != 2 {
        return Ok(false);
    }
    if !is_orchestrator_executor_call(state, provider, caller) {
        return Ok(false);
    }
    let status = meta
        .and_then(|m| m.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if !status.eq_ignore_ascii_case("timeout") {
        return Ok(false);
    }
    let Some(task) = load_task_by_req_id(project_dir, instance, req_id)? else {
        return Ok(false);
    };
    Ok(is_in_flight_status(&task.status))
}

fn is_orchestrator_executor_call(state: &InstanceState, provider: &str, caller: &str) -> bool {
    let Some(orchestrator) = state
        .orchestrator
        .as_deref()
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
    else {
        return false;
    };

    let caller = caller.trim().to_ascii_lowercase();
    let provider = provider.trim().to_ascii_lowercase();
    if caller != orchestrator || provider == orchestrator {
        return false;
    }

    state
        .executors
        .iter()
        .any(|executor| executor.trim().eq_ignore_ascii_case(&provider))
}

fn print_async_submit_notice(
    project_dir: &Path,
    instance: &str,
    provider: &str,
    req_id: &str,
    status: &str,
) {
    match async_submit_stdout_mode().as_str() {
        "minimal" => {
            println!(
                "provider={} req_id={} status={} instance={}",
                provider, req_id, status, instance
            );
        }
        _ => {
            println!(
                "已提交：req_id={} provider={} instance={} status={}",
                req_id, provider, instance, status
            );
            if req_id != "-" {
                println!(
                    "inbox: rccb --project-dir {} inbox --instance {} --req-id {} --latest --limit 5",
                    project_dir.display(),
                    instance,
                    req_id
                );
                println!(
                    "watch: rccb --project-dir {} watch --instance {} --req-id {} --with-provider-log --timeout-s 3",
                    project_dir.display(),
                    instance,
                    req_id
                );
            }
        }
    }
}

fn async_submit_stdout_mode() -> String {
    env::var("RCCB_ASK_ASYNC_STDOUT")
        .ok()
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "full".to_string())
}

fn resolve_response_reply(
    project_dir: &Path,
    instance: &str,
    req_id: Option<&str>,
    meta: Option<&Value>,
    inline_reply: &str,
) -> Result<String> {
    if let Some(path) = meta
        .and_then(|v| v.get("reply_file"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        if let Ok(reply) = fs::read_to_string(path) {
            return Ok(reply);
        }
    }

    if let Some(req_id) = req_id.filter(|v| !v.trim().is_empty()) {
        if let Some(task) = load_task_by_req_id(project_dir, instance, req_id)? {
            if let Some(reply) = task.reply {
                return Ok(reply);
            }
        }
    }

    Ok(inline_reply.to_string())
}

fn cmd_ask_stream(
    project_dir: &Path,
    instance: &str,
    host: &str,
    port: u16,
    req: Value,
    timeout_s: f64,
) -> Result<()> {
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
                    let done_reply = resolve_response_reply(
                        project_dir,
                        instance,
                        event.req_id.as_deref(),
                        event.meta.as_ref(),
                        event.reply.as_deref().unwrap_or_default(),
                    )
                    .unwrap_or_else(|_| event.reply.clone().unwrap_or_default());
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
            let parsed_reply = resolve_response_reply(
                project_dir,
                instance,
                parsed.req_id.as_deref(),
                parsed.meta.as_ref(),
                &parsed.reply,
            )
            .unwrap_or_else(|_| parsed.reply.clone());
            if parsed.exit_code == 0 {
                if !parsed_reply.is_empty() {
                    println!("{}", parsed_reply);
                }
                return Ok(());
            }
            bail!(
                "ask failed: exit_code={} reply={} req_id={}",
                parsed.exit_code,
                parsed_reply,
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

fn resolve_start_debug(override_debug: Option<bool>) -> bool {
    override_debug.unwrap_or(false)
}

fn env_debug_override() -> Option<bool> {
    let raw = env::var("RCCB_DEBUG").ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::path::Path;
    use std::sync::{Mutex, OnceLock};

    use serde_json::json;

    use super::{
        async_submit_stdout_mode, build_debug_watch_command, cleanup_inflight_tasks,
        cleanup_instance_runtime, compact_watch_line, debug_watch_pane_percent,
        enforce_orchestrator_dispatch_guard, ensure_project_bootstrap,
        ensure_project_rule_bootstrap, is_ignorable_pane_command_error, is_in_flight_status,
        is_orchestrator_executor_call, is_terminal_bus_task_event, is_terminal_task_status,
        load_orchestrator_inbox_entries, load_task_by_req_id, orchestrator_guardrail_prompt,
        orchestrator_strict_mode_enabled, provider_start_cmd, render_project_bootstrap_content,
        resolve_debug_watch_provider, resolve_shortcut_restore_providers, run_simple,
        select_watch_req_for_provider, select_watch_req_for_provider_follow,
        should_degrade_timeout_to_pending, split_layout_groups, split_percent_for_equal_stack,
        task_file_for_req_id, watch_bus_enabled, BootstrapMode, RCCB_MANAGED_BEGIN,
        RCCB_MANAGED_END, RCCB_USER_BEGIN, RCCB_USER_END,
    };
    use crate::io_utils::{
        now_unix, now_unix_ms, update_task_status, write_json_pretty, write_state,
    };
    use crate::layout::{
        ensure_project_layout, lock_path, logs_instance_dir, session_instance_dir, state_path,
        tasks_instance_dir, tmp_instance_dir,
    };
    use crate::orchestrator_lock::mark_inflight;
    use crate::types::{AskBusEvent, InstanceState};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn all_test_providers() -> Vec<String> {
        super::SHORTCUT_DEFAULT_PROVIDERS
            .iter()
            .map(|x| x.to_string())
            .collect()
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
    fn env_debug_override_recognizes_explicit_false() {
        let _guard = env_lock().lock().expect("lock env");
        unsafe {
            std::env::set_var("RCCB_DEBUG", "0");
        }
        assert_eq!(super::env_debug_override(), Some(false));
        unsafe {
            std::env::remove_var("RCCB_DEBUG");
        }
    }

    #[test]
    fn resolve_start_debug_defaults_off_without_explicit_override() {
        assert!(!super::resolve_start_debug(None));
        assert!(super::resolve_start_debug(Some(true)));
        assert!(!super::resolve_start_debug(Some(false)));
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
    fn suppresses_sync_reply_for_orchestrator_executor_call() {
        let _guard = env_lock().lock().unwrap();
        let old = std::env::var("RCCB_ORCHESTRATOR_RESULT_CALLBACK").ok();
        unsafe {
            std::env::set_var("RCCB_ORCHESTRATOR_RESULT_CALLBACK", "1");
        }

        let state = InstanceState {
            schema_version: 1,
            instance_id: "default".to_string(),
            project_dir: ".".to_string(),
            pid: 1,
            status: "running".to_string(),
            started_at_unix: 1,
            last_heartbeat_unix: 1,
            stopped_at_unix: None,
            providers: vec!["claude".to_string(), "gemini".to_string()],
            orchestrator: Some("claude".to_string()),
            executors: vec!["gemini".to_string()],
            session_file: None,
            last_task_id: None,
            daemon_host: None,
            daemon_port: None,
            daemon_token: None,
            debug_enabled: false,
        };
        assert!(super::should_suppress_sync_reply_for_orchestrator(
            &state, "gemini", "claude"
        ));

        if let Some(v) = old {
            unsafe {
                std::env::set_var("RCCB_ORCHESTRATOR_RESULT_CALLBACK", v);
            }
        } else {
            unsafe {
                std::env::remove_var("RCCB_ORCHESTRATOR_RESULT_CALLBACK");
            }
        }
    }

    #[test]
    fn keeps_sync_reply_when_result_callback_is_silent() {
        let _guard = env_lock().lock().unwrap();
        let old = std::env::var("RCCB_ORCHESTRATOR_RESULT_CALLBACK").ok();
        unsafe {
            std::env::remove_var("RCCB_ORCHESTRATOR_RESULT_CALLBACK");
        }

        let state = InstanceState {
            schema_version: 1,
            instance_id: "default".to_string(),
            project_dir: ".".to_string(),
            pid: 1,
            status: "running".to_string(),
            started_at_unix: 1,
            last_heartbeat_unix: 1,
            stopped_at_unix: None,
            providers: vec!["claude".to_string(), "gemini".to_string()],
            orchestrator: Some("claude".to_string()),
            executors: vec!["gemini".to_string()],
            session_file: None,
            last_task_id: None,
            daemon_host: None,
            daemon_port: None,
            daemon_token: None,
            debug_enabled: false,
        };
        assert!(!super::should_suppress_sync_reply_for_orchestrator(
            &state, "gemini", "claude"
        ));

        if let Some(v) = old {
            unsafe {
                std::env::set_var("RCCB_ORCHESTRATOR_RESULT_CALLBACK", v);
            }
        }
    }

    #[test]
    fn keeps_sync_reply_for_non_orchestrator_callers() {
        let state = InstanceState {
            schema_version: 1,
            instance_id: "default".to_string(),
            project_dir: ".".to_string(),
            pid: 1,
            status: "running".to_string(),
            started_at_unix: 1,
            last_heartbeat_unix: 1,
            stopped_at_unix: None,
            providers: vec!["claude".to_string(), "gemini".to_string()],
            orchestrator: Some("claude".to_string()),
            executors: vec!["gemini".to_string()],
            session_file: None,
            last_task_id: None,
            daemon_host: None,
            daemon_port: None,
            daemon_token: None,
            debug_enabled: false,
        };
        assert!(!super::should_suppress_sync_reply_for_orchestrator(
            &state, "gemini", "manual"
        ));
    }

    #[test]
    fn async_submit_stdout_mode_defaults_to_full_and_supports_minimal() {
        let _guard = env_lock().lock().unwrap();
        let old = std::env::var("RCCB_ASK_ASYNC_STDOUT").ok();
        unsafe {
            std::env::remove_var("RCCB_ASK_ASYNC_STDOUT");
        }
        assert_eq!(async_submit_stdout_mode(), "full");
        unsafe {
            std::env::set_var("RCCB_ASK_ASYNC_STDOUT", "minimal");
        }
        assert_eq!(async_submit_stdout_mode(), "minimal");
        if let Some(v) = old {
            unsafe {
                std::env::set_var("RCCB_ASK_ASYNC_STDOUT", v);
            }
        } else {
            unsafe {
                std::env::remove_var("RCCB_ASK_ASYNC_STDOUT");
            }
        }
    }

    #[test]
    fn is_orchestrator_executor_call_matches_expected_roles() {
        let state = InstanceState {
            schema_version: 1,
            instance_id: "default".to_string(),
            project_dir: ".".to_string(),
            pid: 1,
            status: "running".to_string(),
            started_at_unix: 1,
            last_heartbeat_unix: 1,
            stopped_at_unix: None,
            providers: vec!["claude".to_string(), "gemini".to_string()],
            orchestrator: Some("claude".to_string()),
            executors: vec!["gemini".to_string()],
            session_file: None,
            last_task_id: None,
            daemon_host: None,
            daemon_port: None,
            daemon_token: None,
            debug_enabled: false,
        };
        assert!(is_orchestrator_executor_call(&state, "gemini", "claude"));
        assert!(!is_orchestrator_executor_call(&state, "claude", "claude"));
        assert!(!is_orchestrator_executor_call(&state, "gemini", "manual"));
    }

    #[test]
    fn should_degrade_timeout_to_pending_when_orchestrator_task_is_still_running() {
        let project =
            std::env::temp_dir().join(format!("rccb-ask-timeout-pending-{}", now_unix_ms()));
        let instance = "default";
        ensure_project_layout(&project).unwrap();
        let task_dir = tasks_instance_dir(&project, instance);
        fs::create_dir_all(&task_dir).unwrap();
        write_json_pretty(
            &task_dir.join("task-1.json"),
            &json!({
                "task_id":"task-1",
                "req_id":"req-1",
                "provider":"gemini",
                "status":"running",
                "created_at_unix": 1
            }),
        )
        .unwrap();

        let state = InstanceState {
            schema_version: 1,
            instance_id: instance.to_string(),
            project_dir: project.display().to_string(),
            pid: 1,
            status: "running".to_string(),
            started_at_unix: 1,
            last_heartbeat_unix: 1,
            stopped_at_unix: None,
            providers: vec!["claude".to_string(), "gemini".to_string()],
            orchestrator: Some("claude".to_string()),
            executors: vec!["gemini".to_string()],
            session_file: None,
            last_task_id: None,
            daemon_host: None,
            daemon_port: None,
            daemon_token: None,
            debug_enabled: false,
        };

        assert!(should_degrade_timeout_to_pending(
            &project,
            instance,
            &state,
            "gemini",
            "claude",
            2,
            Some(&json!({"status":"timeout"})),
            "req-1"
        )
        .unwrap());

        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn orchestrator_dispatch_guard_blocks_main_orchestrator_parallel_ask() {
        let _guard = env_lock().lock().unwrap();
        let project = std::env::temp_dir().join(format!("rccb-ask-guard-main-{}", now_unix_ms()));
        let instance = "default";
        ensure_project_layout(&project).unwrap();
        let state = InstanceState {
            schema_version: 1,
            instance_id: instance.to_string(),
            project_dir: project.display().to_string(),
            pid: 1,
            status: "running".to_string(),
            started_at_unix: 1,
            last_heartbeat_unix: 1,
            stopped_at_unix: None,
            providers: vec!["claude".to_string(), "gemini".to_string()],
            orchestrator: Some("claude".to_string()),
            executors: vec!["gemini".to_string()],
            session_file: None,
            last_task_id: None,
            daemon_host: None,
            daemon_port: None,
            daemon_token: None,
            debug_enabled: false,
        };
        write_state(&state_path(&project, instance), &state).unwrap();
        mark_inflight(&project, instance, "claude", "gemini", "req-1", "running").unwrap();

        unsafe {
            std::env::set_var("RCCB_PROVIDER_ROLE", "orchestrator");
            std::env::set_var("RCCB_PROVIDER_AGENT", "orchestrator");
            std::env::remove_var("RCCB_ORCHESTRATOR_ALLOW_PARALLEL");
        }
        let err =
            enforce_orchestrator_dispatch_guard(&project, instance, &state, "gemini", "claude")
                .unwrap_err();
        assert!(err.to_string().contains("主编排者等待态已生效"));

        unsafe {
            std::env::remove_var("RCCB_PROVIDER_ROLE");
            std::env::remove_var("RCCB_PROVIDER_AGENT");
        }
        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn orchestrator_dispatch_guard_allows_delegate_agent_parallel_ask() {
        let _guard = env_lock().lock().unwrap();
        let project =
            std::env::temp_dir().join(format!("rccb-ask-guard-delegate-{}", now_unix_ms()));
        let instance = "default";
        ensure_project_layout(&project).unwrap();
        let state = InstanceState {
            schema_version: 1,
            instance_id: instance.to_string(),
            project_dir: project.display().to_string(),
            pid: 1,
            status: "running".to_string(),
            started_at_unix: 1,
            last_heartbeat_unix: 1,
            stopped_at_unix: None,
            providers: vec!["claude".to_string(), "gemini".to_string()],
            orchestrator: Some("claude".to_string()),
            executors: vec!["gemini".to_string()],
            session_file: None,
            last_task_id: None,
            daemon_host: None,
            daemon_port: None,
            daemon_token: None,
            debug_enabled: false,
        };
        write_state(&state_path(&project, instance), &state).unwrap();
        mark_inflight(&project, instance, "claude", "gemini", "req-1", "running").unwrap();

        unsafe {
            std::env::set_var("RCCB_PROVIDER_ROLE", "orchestrator");
            std::env::set_var("RCCB_PROVIDER_AGENT", "delegate-researcher");
            std::env::remove_var("RCCB_ORCHESTRATOR_ALLOW_PARALLEL");
        }
        enforce_orchestrator_dispatch_guard(&project, instance, &state, "gemini", "claude")
            .unwrap();

        unsafe {
            std::env::remove_var("RCCB_PROVIDER_ROLE");
            std::env::remove_var("RCCB_PROVIDER_AGENT");
        }
        let _ = fs::remove_dir_all(&project);
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
    fn rule_bootstrap_creates_expected_files() {
        let project = std::env::temp_dir().join(format!("rccb-rules-{}", now_unix_ms()));
        ensure_project_layout(&project).unwrap();

        let written =
            ensure_project_bootstrap(&project, BootstrapMode::MissingOnly, &all_test_providers())
                .unwrap();

        assert!(written
            .rule_templates
            .iter()
            .any(|p| p.ends_with("AGENTS.md")));
        assert!(written.config_path.ends_with("config.example.json"));
        assert!(project.join("AGENTS.md").exists());
        assert!(project.join("CLAUDE.md").exists());
        assert!(project.join("GEMINI.md").exists());
        let gemini_rules = fs::read_to_string(project.join("GEMINI.md")).unwrap();
        assert!(gemini_rules.contains("建议工作流"));
        assert!(gemini_rules.contains("至少执行两轮调研"));
        let agents_skill =
            fs::read_to_string(project.join(".agents/skills/rccb-delegate/SKILL.md")).unwrap();
        assert!(agents_skill.starts_with("---\nname: rccb-delegate\n"));
        let config_template =
            fs::read_to_string(project.join(".rccb/config.example.json")).unwrap();
        assert!(config_template.contains("首个 provider 作为编排者"));
        let orchestrator_agent =
            fs::read_to_string(project.join(".claude/agents/orchestrator.md")).unwrap();
        assert!(orchestrator_agent.contains("# 编排者"));
        assert!(project
            .join(".agents/skills/rccb-delegate/SKILL.md")
            .exists());
        assert!(project.join(".agents/skills/rccb-audit/SKILL.md").exists());
        assert!(project
            .join(".agents/skills/rccb-research-verify/SKILL.md")
            .exists());
        assert!(project
            .join(".opencode/skills/rccb-delegate/SKILL.md")
            .exists());
        assert!(project
            .join(".factory/skills/rccb-delegate/SKILL.md")
            .exists());
        assert!(project.join(".opencode/commands/rccb-code.md").exists());
        assert!(project.join(".opencode/agents/coder.md").exists());
        assert!(project.join(".claude/agents/orchestrator.md").exists());
        assert!(project.join(".factory/commands/rccb-research.md").exists());
        assert!(project.join(".factory/rules/rccb-core.md").exists());
        assert!(project.join(".factory/droids/researcher.md").exists());
        assert!(project.join(".factory/settings.local.json").exists());
        assert!(project.join(".claude/settings.local.json").exists());
        assert!(project.join(".claude/commands/rccb-research.md").exists());
        assert!(project.join(".rccb/providers/codex.example.json").exists());
        assert!(project
            .join(".rccb/providers/gemini.trustedFolders.json")
            .exists());
        assert!(project.join(".rccb/bin/codex").exists());
        assert!(project.join(".rccb/bin/claude").exists());

        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn rule_bootstrap_refresh_keeps_user_section() {
        let project = std::env::temp_dir().join(format!("rccb-rules-user-{}", now_unix_ms()));
        ensure_project_layout(&project).unwrap();

        let agents_path = project.join("AGENTS.md");
        fs::write(
            &agents_path,
            format!(
                "{RCCB_MANAGED_BEGIN}\nold\n{RCCB_MANAGED_END}\n\n{RCCB_USER_BEGIN}\n自定义规则\n{RCCB_USER_END}\n"
            ),
        )
        .unwrap();

        ensure_project_rule_bootstrap(
            &project,
            BootstrapMode::RefreshGenerated,
            &all_test_providers(),
        )
        .unwrap();
        let updated = fs::read_to_string(&agents_path).unwrap();
        assert!(updated.contains("自定义规则"));
        assert!(updated.contains("gemini"));
        assert!(updated.contains("codex"));

        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn bootstrap_refresh_overwrites_generated_templates() {
        let project =
            std::env::temp_dir().join(format!("rccb-bootstrap-refresh-{}", now_unix_ms()));
        ensure_project_layout(&project).unwrap();

        let config_path = project.join(".rccb/config.example.json");
        let profile_path = project.join(".rccb/providers/codex.example.json");
        fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        fs::write(&config_path, "{\"stale\":true}\n").unwrap();
        fs::write(&profile_path, "{\"provider\":\"codex\",\"stale\":true}\n").unwrap();

        ensure_project_bootstrap(
            &project,
            BootstrapMode::RefreshGenerated,
            &all_test_providers(),
        )
        .unwrap();

        let config = fs::read_to_string(&config_path).unwrap();
        let profile = fs::read_to_string(&profile_path).unwrap();
        let trusted =
            fs::read_to_string(project.join(".rccb/providers/gemini.trustedFolders.json")).unwrap();
        let droid_settings =
            fs::read_to_string(project.join(".factory/settings.local.json")).unwrap();
        let claude_settings =
            fs::read_to_string(project.join(".claude/settings.local.json")).unwrap();
        assert!(config.contains("default_specialties"));
        assert!(profile.contains("\"RCCB_TASK_ID\""));
        assert!(trusted.contains("\"TRUST_FOLDER\""));
        assert!(trusted.contains(&project.display().to_string()));
        assert!(droid_settings.contains("\"autonomyMode\": \"auto-high\""));
        assert!(claude_settings.contains("Bash(rccb:*)"));
        assert!(claude_settings.contains("RCCB_ASK_ASYNC_STDOUT=minimal"));

        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn bootstrap_missing_only_merges_claude_settings_allowlist() {
        let project =
            std::env::temp_dir().join(format!("rccb-bootstrap-claude-settings-{}", now_unix_ms()));
        ensure_project_layout(&project).unwrap();
        let settings_path = project.join(".claude/settings.local.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(
            &settings_path,
            r#"{
  "permissions": {
    "allow": ["WebFetch", "Bash(custom-tool:*)"]
  },
  "custom": true
}
"#,
        )
        .unwrap();

        ensure_project_bootstrap(&project, BootstrapMode::MissingOnly, &all_test_providers())
            .unwrap();

        let merged = fs::read_to_string(&settings_path).unwrap();
        assert!(merged.contains("\"WebFetch\""));
        assert!(merged.contains("\"Bash(custom-tool:*)\""));
        assert!(merged.contains("\"Bash(rccb:*)\""));
        assert!(merged.contains("RCCB_ASK_ASYNC_STDOUT=minimal"));
        assert!(merged.contains("\"custom\": true"));

        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn bootstrap_only_generates_selected_provider_rules() {
        let project = std::env::temp_dir().join(format!("rccb-bootstrap-subset-{}", now_unix_ms()));
        ensure_project_layout(&project).unwrap();
        let providers = vec!["claude".to_string(), "gemini".to_string()];

        ensure_project_bootstrap(&project, BootstrapMode::RefreshGenerated, &providers).unwrap();

        assert!(project.join("AGENTS.md").exists());
        assert!(project.join("CLAUDE.md").exists());
        assert!(project.join("GEMINI.md").exists());
        assert!(!project.join(".opencode").exists());
        assert!(!project.join(".factory").exists());
        assert!(!project.join(".agents").exists());
        assert!(project.join(".rccb/bin/claude").exists());
        assert!(project.join(".rccb/bin/gemini").exists());
        assert!(!project.join(".rccb/bin/opencode").exists());
        let config = fs::read_to_string(project.join(".rccb/config.example.json")).unwrap();
        assert!(config.contains("\"claude\""));
        assert!(config.contains("\"gemini\""));
        assert!(!config.contains("\"opencode\""));

        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn provider_wrappers_include_project_permission_defaults() {
        let claude = super::build_provider_wrapper_script("claude").expect("claude wrapper");
        let codex = super::build_provider_wrapper_script("codex").expect("codex wrapper");
        let gemini = super::build_provider_wrapper_script("gemini").expect("gemini wrapper");
        let droid = super::build_provider_wrapper_script("droid").expect("droid wrapper");

        assert!(claude.starts_with("#!/usr/bin/env sh"));
        assert!(claude.contains("role=\"${RCCB_PROVIDER_ROLE:-executor}\""));
        assert!(claude.contains("--allowedTools \"Read Grep Glob LS Task Bash(rccb:*)"));
        assert!(claude.contains("--disallowedTools \"Edit MultiEdit Write NotebookEdit\""));
        assert!(claude.contains("--permission-mode bypassPermissions"));
        assert!(claude.contains("--dangerously-skip-permissions"));
        assert!(!claude.contains("[["));
        assert!(codex.contains("-a on-request -s workspace-write"));
        assert!(codex.contains("-a never -s workspace-write"));
        assert!(gemini.contains("GEMINI_CLI_TRUSTED_FOLDERS_PATH"));
        assert!(gemini.contains("--approval-mode default"));
        assert!(gemini.contains("--approval-mode yolo"));
        assert!(droid.contains("--skip-permissions-unsafe"));
        assert!(droid.contains("if [ \"$role\" = \"orchestrator\" ]"));
        assert!(droid.contains(".factory/settings.local.json"));
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
        let prompt = orchestrator_guardrail_prompt(
            Path::new("/tmp/rccb-proj"),
            "claude",
            &["codex".to_string(), "gemini".to_string()],
        );
        assert!(prompt.contains("不要自己执行 bash"));
        assert!(prompt.contains("codex, gemini"));
        assert!(prompt.contains("--caller claude"));
        assert!(prompt.contains("后台 inbox"));
        assert!(prompt.contains("--latest --limit 5"));
        assert!(prompt.contains("`.reply.md`"));
        assert!(prompt.contains("opencode=编码"));
        assert!(prompt.contains("先 gemini 至少两轮调研"));
        assert!(prompt.contains("再 codex 复核关键结论"));
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
    fn run_simple_captures_command_output_in_error() {
        let err = run_simple(
            "/bin/sh",
            &["-c", "echo pane-missing >&2; echo noisy-out; exit 7"],
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("status=exit status: 7"));
        assert!(text.contains("stdout=`noisy-out`"));
        assert!(text.contains("stderr=`pane-missing`"));
    }

    #[test]
    fn pane_missing_errors_are_ignorable_during_cleanup() {
        let err =
            run_simple("/bin/sh", &["-c", "echo can't find pane: %47 >&2; exit 1"]).unwrap_err();
        assert!(is_ignorable_pane_command_error(&err));

        let err = run_simple("/bin/sh", &["-c", "echo fatal boom >&2; exit 1"]).unwrap_err();
        assert!(!is_ignorable_pane_command_error(&err));
    }

    #[test]
    fn provider_start_cmd_prefers_project_wrapper_and_role_env() {
        let _guard = env_lock().lock().unwrap();
        let old_ccb = std::env::var("RCCB_USE_BRIDGE_PROVIDER_LAUNCH").ok();
        unsafe {
            std::env::remove_var("RCCB_USE_BRIDGE_PROVIDER_LAUNCH");
        }
        let project = std::env::temp_dir().join(format!("rccb-start-wrapper-{}", now_unix_ms()));
        ensure_project_bootstrap(
            &project,
            BootstrapMode::RefreshGenerated,
            &all_test_providers(),
        )
        .expect("bootstrap");
        let cmd = provider_start_cmd(&project, "default", "opencode");
        if let Some(v) = old_ccb {
            unsafe {
                std::env::set_var("RCCB_USE_BRIDGE_PROVIDER_LAUNCH", v);
            }
        } else {
            unsafe {
                std::env::remove_var("RCCB_USE_BRIDGE_PROVIDER_LAUNCH");
            }
        }
        assert!(cmd.contains("RCCB_PROVIDER_AGENT"));
        assert!(cmd.contains("RCCB_PROVIDER_SPECIALTY"));
        assert!(cmd.contains(".rccb/bin/opencode"));
        assert!(!cmd.contains("tail -n0 -F"));
        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn legacy_zsh_wrapper_is_marked_for_refresh() {
        let project = std::env::temp_dir().join(format!("rccb-wrapper-refresh-{}", now_unix_ms()));
        ensure_project_layout(&project).unwrap();
        let wrapper = project.join(".rccb").join("bin").join("gemini");
        fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
        fs::write(
            &wrapper,
            "#!/usr/bin/env zsh\nset -euo pipefail\nif [[ \"$role\" == \"orchestrator\" ]]; then\n  exit 0\nfi\n",
        )
        .unwrap();

        assert!(super::provider_wrapper_needs_refresh(&wrapper).unwrap());
        let refreshed =
            super::refresh_legacy_provider_wrappers(&project, &["gemini".to_string()]).unwrap();
        assert_eq!(refreshed.len(), 1);
        let raw = fs::read_to_string(&wrapper).unwrap();
        assert!(raw.starts_with("#!/usr/bin/env sh"));
        assert!(!raw.contains("[["));

        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn validate_provider_prerequisites_reports_missing_cli() {
        let _guard = env_lock().lock().expect("lock env");
        let old_path = std::env::var("PATH").ok();
        unsafe {
            std::env::set_var("PATH", "");
        }
        let project = Path::new("/tmp/rccb-preflight");
        let err =
            super::validate_provider_prerequisites(project, &["gemini".to_string()]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("启动前检查失败"));
        assert!(msg.contains("gemini"));
        assert!(msg.contains("RCCB_GEMINI_START_CMD"));
        if let Some(v) = old_path {
            unsafe {
                std::env::set_var("PATH", v);
            }
        } else {
            unsafe {
                std::env::remove_var("PATH");
            }
        }
    }

    #[test]
    fn resolve_shell_path_falls_back_when_env_shell_is_invalid() {
        let _guard = env_lock().lock().expect("lock env");
        let old_shell = std::env::var("SHELL").ok();
        unsafe {
            std::env::set_var("SHELL", "/definitely/missing/rccb-shell");
        }
        let shell = super::resolve_shell_path();
        assert!(shell == "/bin/bash" || shell == "/bin/sh" || shell == "sh");
        if let Some(v) = old_shell {
            unsafe {
                std::env::set_var("SHELL", v);
            }
        } else {
            unsafe {
                std::env::remove_var("SHELL");
            }
        }
    }

    #[test]
    fn render_project_bootstrap_content_injects_project_specific_command() {
        let project = Path::new("/tmp/rccb-proj");
        let rendered = render_project_bootstrap_content(
            project,
            "执行：rccb --project-dir . ask --instance default --provider codex --caller claude \"hi\"",
        );
        assert!(!rendered.contains("rccb --project-dir ."));
        assert!(rendered.contains("--project-dir"));
        assert!(rendered.contains("/tmp/rccb-proj"));
    }

    #[test]
    fn resolve_shortcut_restore_providers_filters_unavailable_clis() {
        let _guard = env_lock().lock().unwrap();
        let old_path = std::env::var("PATH").ok();
        let old_claude = std::env::var("RCCB_CLAUDE_START_CMD").ok();
        let old_codex = std::env::var("RCCB_CODEX_START_CMD").ok();
        unsafe {
            std::env::set_var("PATH", "");
            std::env::set_var("RCCB_CLAUDE_START_CMD", "claude");
            std::env::remove_var("RCCB_CODEX_START_CMD");
        }

        let project = std::env::temp_dir().join(format!("rccb-restore-{}", now_unix_ms()));
        ensure_project_layout(&project).unwrap();
        write_state(
            &state_path(&project, "default"),
            &InstanceState {
                schema_version: 1,
                instance_id: "default".to_string(),
                project_dir: project.display().to_string(),
                pid: 1,
                status: "stopped".to_string(),
                started_at_unix: 0,
                last_heartbeat_unix: 0,
                stopped_at_unix: Some(0),
                providers: vec!["claude".to_string(), "codex".to_string()],
                orchestrator: Some("claude".to_string()),
                executors: vec!["codex".to_string()],
                session_file: None,
                last_task_id: None,
                daemon_host: None,
                daemon_port: None,
                daemon_token: None,
                debug_enabled: false,
            },
        )
        .unwrap();

        let providers = resolve_shortcut_restore_providers(&project).unwrap();
        assert_eq!(providers, vec!["claude".to_string()]);

        if let Some(v) = old_path {
            unsafe { std::env::set_var("PATH", v) };
        } else {
            unsafe { std::env::remove_var("PATH") };
        }
        if let Some(v) = old_claude {
            unsafe { std::env::set_var("RCCB_CLAUDE_START_CMD", v) };
        } else {
            unsafe { std::env::remove_var("RCCB_CLAUDE_START_CMD") };
        }
        if let Some(v) = old_codex {
            unsafe { std::env::set_var("RCCB_CODEX_START_CMD", v) };
        } else {
            unsafe { std::env::remove_var("RCCB_CODEX_START_CMD") };
        }
        let _ = fs::remove_dir_all(&project);
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

    #[test]
    fn cleanup_instance_runtime_removes_runtime_dirs_but_keeps_history() {
        let project = std::env::temp_dir().join(format!("rccb-runtime-clean-{}", now_unix_ms()));
        let instance = "default";
        ensure_project_layout(&project).unwrap();

        write_state(
            &state_path(&project, instance),
            &InstanceState {
                schema_version: 1,
                instance_id: instance.to_string(),
                project_dir: project.display().to_string(),
                pid: 999_999,
                status: "stopped".to_string(),
                started_at_unix: 0,
                last_heartbeat_unix: 0,
                stopped_at_unix: Some(0),
                providers: vec!["claude".to_string()],
                orchestrator: Some("claude".to_string()),
                executors: vec![],
                session_file: None,
                last_task_id: None,
                daemon_host: None,
                daemon_port: None,
                daemon_token: None,
                debug_enabled: false,
            },
        )
        .unwrap();

        fs::write(lock_path(&project, instance), b"lock").unwrap();
        fs::create_dir_all(session_instance_dir(&project, instance)).unwrap();
        fs::write(
            session_instance_dir(&project, instance).join("session.json"),
            b"{}",
        )
        .unwrap();
        fs::create_dir_all(tmp_instance_dir(&project, instance).join("launcher")).unwrap();
        fs::write(
            tmp_instance_dir(&project, instance)
                .join("launcher")
                .join("meta.json"),
            b"{}",
        )
        .unwrap();

        let task_dir = tasks_instance_dir(&project, instance);
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("task-1.json"), b"{}").unwrap();
        let log_dir = logs_instance_dir(&project, instance);
        fs::create_dir_all(&log_dir).unwrap();
        fs::write(log_dir.join("daemon.log"), b"keep").unwrap();

        cleanup_instance_runtime(&project, instance).unwrap();

        assert!(!state_path(&project, instance).exists());
        assert!(!lock_path(&project, instance).exists());
        assert!(!session_instance_dir(&project, instance).exists());
        assert!(!tmp_instance_dir(&project, instance).exists());
        assert!(task_dir.exists());
        assert!(log_dir.exists());

        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn cleanup_instance_runtime_refuses_live_instance() {
        let project =
            std::env::temp_dir().join(format!("rccb-runtime-live-guard-{}", now_unix_ms()));
        let instance = "default";
        ensure_project_layout(&project).unwrap();

        write_state(
            &state_path(&project, instance),
            &InstanceState {
                schema_version: 1,
                instance_id: instance.to_string(),
                project_dir: project.display().to_string(),
                pid: std::process::id(),
                status: "running".to_string(),
                started_at_unix: 0,
                last_heartbeat_unix: 0,
                stopped_at_unix: None,
                providers: vec!["claude".to_string()],
                orchestrator: Some("claude".to_string()),
                executors: vec![],
                session_file: None,
                last_task_id: None,
                daemon_host: None,
                daemon_port: None,
                daemon_token: None,
                debug_enabled: false,
            },
        )
        .unwrap();

        let err = cleanup_instance_runtime(&project, instance).unwrap_err();
        assert!(err.to_string().contains("实例仍在运行"));
        assert!(state_path(&project, instance).exists());

        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn load_orchestrator_inbox_entries_reads_jsonl_entries() {
        let project = std::env::temp_dir().join(format!("rccb-inbox-{}", now_unix_ms()));
        let instance = "default";
        ensure_project_layout(&project).unwrap();
        let inbox = tmp_instance_dir(&project, instance)
            .join("orchestrator")
            .join("claude.jsonl");
        fs::create_dir_all(inbox.parent().unwrap()).unwrap();
        fs::write(
            &inbox,
            concat!(
                "{\"kind\":\"status\",\"req_id\":\"req-1\",\"executor\":\"gemini\",\"status\":\"running\",\"message\":\"still working\",\"ts_unix\":1}\n",
                "{\"kind\":\"result\",\"req_id\":\"req-1\",\"executor\":\"gemini\",\"status\":\"completed\",\"exit_code\":0,\"reply\":\"done\",\"reply_file\":\"/tmp/reply.md\",\"ts_unix\":2}\n"
            ),
        )
        .unwrap();

        let items = load_orchestrator_inbox_entries(&project, instance, "claude").unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, "status");
        assert_eq!(items[1].kind, "result");
        assert_eq!(items[1].reply.as_deref(), Some("done"));
        assert_eq!(items[1].reply_file.as_deref(), Some("/tmp/reply.md"));

        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn collapse_inbox_entries_latest_keeps_latest_status_and_result_per_req() {
        let items = vec![
            super::InboxEntryView {
                instance: "default".to_string(),
                orchestrator: "claude".to_string(),
                kind: "status".to_string(),
                req_id: Some("req-1".to_string()),
                executor: Some("gemini".to_string()),
                caller: Some("claude".to_string()),
                status: Some("running".to_string()),
                exit_code: None,
                ts_unix: Some(1),
                message: Some("started".to_string()),
                reply: None,
                reply_file: None,
            },
            super::InboxEntryView {
                instance: "default".to_string(),
                orchestrator: "claude".to_string(),
                kind: "status".to_string(),
                req_id: Some("req-1".to_string()),
                executor: Some("gemini".to_string()),
                caller: Some("claude".to_string()),
                status: Some("running".to_string()),
                exit_code: None,
                ts_unix: Some(2),
                message: Some("still working".to_string()),
                reply: None,
                reply_file: None,
            },
            super::InboxEntryView {
                instance: "default".to_string(),
                orchestrator: "claude".to_string(),
                kind: "result".to_string(),
                req_id: Some("req-1".to_string()),
                executor: Some("gemini".to_string()),
                caller: Some("claude".to_string()),
                status: Some("completed".to_string()),
                exit_code: Some(0),
                ts_unix: Some(3),
                message: None,
                reply: Some("done".to_string()),
                reply_file: Some("/tmp/reply.md".to_string()),
            },
            super::InboxEntryView {
                instance: "default".to_string(),
                orchestrator: "claude".to_string(),
                kind: "status".to_string(),
                req_id: Some("req-2".to_string()),
                executor: Some("opencode".to_string()),
                caller: Some("claude".to_string()),
                status: Some("running".to_string()),
                exit_code: None,
                ts_unix: Some(4),
                message: Some("queued".to_string()),
                reply: None,
                reply_file: None,
            },
        ];

        let got = super::collapse_inbox_entries_latest(items);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].req_id.as_deref(), Some("req-1"));
        assert_eq!(got[0].kind, "status");
        assert_eq!(got[0].message.as_deref(), Some("still working"));
        assert_eq!(got[1].req_id.as_deref(), Some("req-1"));
        assert_eq!(got[1].kind, "result");
        assert_eq!(got[2].req_id.as_deref(), Some("req-2"));
        assert_eq!(got[2].kind, "status");
    }
}

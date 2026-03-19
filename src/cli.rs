use std::path::PathBuf;

use clap::{Parser, Subcommand};

const HELP_EXAMPLES: &str = r#"示例：
  1) 一键恢复默认实例（优先恢复上次 provider 布局）：
     rccb

  2) 一键启动（第一个为编排者）：
     rccb claude opencode gemini codex droid

  3) 一键启动并开启调试：
     RCCB_DEBUG=1 rccb claude codex gemini opencode droid

  4) 显式启动实例：
     rccb --project-dir . start --instance team-a --debug claude codex gemini

  5) 发送请求：
     rccb --project-dir . ask --instance team-a --provider codex --caller claude "请检查边界条件"

  6) 流式请求：
     rccb --project-dir . ask --instance team-a --provider gemini --caller claude --stream "持续输出进度"

  7) 异步请求 + 追踪：
     rccb --project-dir . ask --instance team-a --provider opencode --caller claude --async --req-id req-1 "后台执行"
     rccb --project-dir . await --instance team-a --req-id req-1
     rccb --project-dir . watch --instance team-a --req-id req-1 --with-provider-log --with-debug-log
     rccb --project-dir . watch --instance team-a --provider opencode --with-provider-log
     rccb --project-dir . watch --instance team-a --provider opencode --with-provider-log --follow

  8) 查看运行态：
     rccb --project-dir . status --as-json
     rccb --project-dir . mounted --as-json
     rccb --project-dir . tasks --instance team-a --limit 50 --as-json
     rccb --project-dir . inbox --instance team-a --orchestrator claude --limit 20

  9) 自动更新：
     rccb --project-dir . update check
     rccb --project-dir . update apply
     rccb --project-dir . update apply --install-path /usr/local/bin/rccb

  10) 兼容旧命令（统一入口）：
     rccb cask "..."
     rccb cping
     rccb cpend
"#;

#[derive(Parser, Debug)]
#[command(
    name = "rccb",
    version,
    about = "RCCB：Rust 重构版实时协同桥（项目级绑定、多实例、可观测）",
    long_about = "RCCB 是对桥接核心能力的 Rust 重构：\n- 项目级 .rccb 绑定（不依赖全局状态）\n- 多 provider 编排（首个为编排者）\n- ask.* 协议通信（同步/流式/异步/取消）\n- 完整日志与调试开关\n- 支持 tmux/wezterm 场景下的快捷启动与 pane 编排",
    after_long_help = HELP_EXAMPLES
)]
pub struct Cli {
    #[arg(
        long,
        default_value = ".",
        help = "项目目录（状态存放在 <project>/.rccb）"
    )]
    pub project_dir: PathBuf,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    #[command(
        about = "初始化项目目录结构",
        long_about = "初始化 .rccb 目录、默认配置模板、native provider 配置模板。"
    )]
    Init {
        #[arg(long, default_value_t = false, help = "强制覆盖已有模板文件")]
        force: bool,
    },

    #[command(
        about = "前台启动 daemon 实例",
        long_about = "启动指定实例的 RCCB daemon（前台运行）。\n建议在需要精细参数控制时使用。"
    )]
    Start {
        #[arg(long, default_value = "default", help = "实例 ID")]
        instance: String,

        #[arg(long, default_value_t = 5, help = "心跳间隔（秒）")]
        heartbeat_secs: u64,

        #[arg(
            long,
            default_value = "127.0.0.1:0",
            help = "daemon 监听地址（host:port，port=0 表示自动分配）"
        )]
        listen: String,

        #[arg(long, help = "可选：写入 .rccb/tasks 的初始任务文本")]
        task: Option<String>,

        #[arg(long, default_value_t = false, help = "启动时开启完整调试日志")]
        debug: bool,

        #[arg(
            value_name = "PROVIDER",
            num_args = 0..,
            help = "Provider 启动顺序（第一个为编排者）"
        )]
        providers: Vec<String>,
    },

    #[command(
        about = "检查或执行自更新",
        long_about = "面向发布版二进制的自动更新能力：可检查 GitHub Release 最新版本，并下载校验后安装到指定路径。"
    )]
    Update {
        #[command(subcommand)]
        action: UpdateCommand,
    },

    #[command(
        about = "查看实例状态",
        long_about = "查看实例状态、编排关系、daemon 地址与调试状态。"
    )]
    Status {
        #[arg(long, help = "仅查看指定实例")]
        instance: Option<String>,

        #[arg(long, default_value_t = false, help = "以 JSON 输出")]
        as_json: bool,
    },

    #[command(
        about = "查看 mounted 状态",
        long_about = "查看 provider 是否 mounted。\nmounted 定义：session 文件存在且 daemon 在线。"
    )]
    Mounted {
        #[arg(long, help = "仅查看指定实例")]
        instance: Option<String>,

        #[arg(long, default_value_t = false, help = "以 JSON 输出")]
        as_json: bool,
    },

    #[command(
        about = "查看任务列表",
        long_about = "查看任务生命周期记录（queued/running/completed/failed/...），便于排障与审计。"
    )]
    Tasks {
        #[arg(long, help = "仅查看指定实例")]
        instance: Option<String>,

        #[arg(long, default_value_t = 20, help = "返回条数上限")]
        limit: usize,

        #[arg(long, default_value_t = false, help = "以 JSON 输出")]
        as_json: bool,
    },

    #[command(
        about = "查看编排者后台 inbox",
        long_about = "查看静默模式下写入编排者后台 inbox 的 notice 记录，可按 orchestrator/req_id/kind 过滤。"
    )]
    Inbox {
        #[arg(long, default_value = "default", help = "实例 ID")]
        instance: String,

        #[arg(long, help = "编排者 provider；省略时优先从实例状态推断")]
        orchestrator: Option<String>,

        #[arg(long, help = "仅查看指定 req_id")]
        req_id: Option<String>,

        #[arg(long, help = "仅查看指定 kind（status/progress/result）")]
        kind: Option<String>,

        #[arg(
            long,
            default_value_t = false,
            help = "按 req_id/结果类型折叠，只保留最新状态与最新结果"
        )]
        latest: bool,

        #[arg(long, default_value_t = 20, help = "返回条数上限")]
        limit: usize,

        #[arg(long, default_value_t = false, help = "以 JSON 输出")]
        as_json: bool,
    },

    #[command(
        about = "实时追踪请求",
        long_about = "按 req_id 追踪任务状态变化，可选跟踪 provider/debug 日志。常用于异步任务观察。"
    )]
    Watch {
        #[arg(long, default_value = "default", help = "实例 ID")]
        instance: String,

        #[arg(long, help = "要追踪的 req_id（可选）")]
        req_id: Option<String>,

        #[arg(long, help = "按 provider 追踪最新任务（与 --req-id 二选一）")]
        provider: Option<String>,

        #[arg(
            long,
            default_value_t = false,
            help = "全局追踪所有 provider/req_id（适合 debug pane）"
        )]
        all: bool,

        #[arg(long, default_value_t = 800, help = "轮询间隔（毫秒）")]
        poll_ms: u64,

        #[arg(long, default_value_t = 120.0, help = "超时时间（秒，<=0 表示不超时）")]
        timeout_s: f64,

        #[arg(
            long,
            default_value_t = false,
            help = "持续追踪模式：任务结束后继续等待下一条（通常与 --provider 搭配）"
        )]
        follow: bool,

        #[arg(
            long,
            default_value_t = false,
            help = "同时输出 provider 日志中与 req_id 相关的增量行"
        )]
        with_provider_log: bool,

        #[arg(
            long,
            default_value_t = false,
            help = "同时输出 debug 日志中与 req_id 相关的增量行"
        )]
        with_debug_log: bool,

        #[arg(
            long,
            default_value_t = false,
            help = "使用适合 pane 前端的紧凑输出样式"
        )]
        pane_ui: bool,

        #[arg(long, default_value_t = false, help = "以 JSON 事件流输出")]
        as_json: bool,
    },

    #[command(
        about = "停止实例",
        long_about = "停止指定实例 daemon（优先走协议优雅关闭）。"
    )]
    Stop {
        #[arg(long, default_value = "default", help = "实例 ID")]
        instance: String,
    },

    #[command(
        about = "检测连通性",
        long_about = "向实例 daemon 发送 ping 并验证响应。"
    )]
    Ping {
        #[arg(long, default_value = "default", help = "实例 ID")]
        instance: String,

        #[arg(long, default_value_t = 1.0, help = "请求超时（秒）")]
        timeout_s: f64,
    },

    #[command(about = "取消请求", long_about = "按 req_id 取消执行中的请求。")]
    Cancel {
        #[arg(long, default_value = "default", help = "实例 ID")]
        instance: String,

        #[arg(long, help = "要取消的 req_id")]
        req_id: String,
    },

    #[command(
        about = "发送 ask 请求",
        long_about = "向 daemon 发送 ask.request。\n支持同步、流式（--stream）和异步提交（--async）。"
    )]
    Ask {
        #[arg(long, default_value = "default", help = "实例 ID")]
        instance: String,

        #[arg(long, help = "目标 provider（claude/codex/gemini/opencode/droid）")]
        provider: String,

        #[arg(long, default_value = "manual", help = "调用方标识（caller）")]
        caller: String,

        #[arg(long, default_value_t = 300.0, help = "请求超时（秒）")]
        timeout_s: f64,

        #[arg(
            long,
            default_value_t = false,
            help = "静默模式（传给 provider 执行层）"
        )]
        quiet: bool,

        #[arg(long, default_value_t = false, help = "开启流式响应（实时输出）")]
        stream: bool,

        #[arg(
            long = "async",
            default_value_t = false,
            help = "异步提交，立即返回 req_id"
        )]
        async_submit: bool,

        #[arg(
            long,
            default_value_t = false,
            help = "异步提交后继续阻塞等待该 req_id 进入终态"
        )]
        await_terminal: bool,

        #[arg(long, help = "自定义 req_id（可选）")]
        req_id: Option<String>,

        #[arg(
            value_name = "MESSAGE",
            num_args = 0..,
            help = "消息文本；留空时从 stdin 读取"
        )]
        message: Vec<String>,
    },

    #[command(
        about = "等待请求进入终态",
        long_about = "按 req_id 阻塞等待任务进入 completed/failed/timeout/canceled/incomplete 等终态。适合子代理在派单后安静等待最终结果。"
    )]
    Await {
        #[arg(long, default_value = "default", help = "实例 ID")]
        instance: String,

        #[arg(long, help = "要等待的 req_id")]
        req_id: String,

        #[arg(long, default_value_t = 0.0, help = "等待超时（秒，<=0 表示不超时）")]
        timeout_s: f64,

        #[arg(long, default_value_t = false, help = "以 JSON 输出")]
        as_json: bool,
    },

    #[command(about = "发送 IM 消息", long_about = "发送通知到飞书或 Telegram。")]
    Send {
        #[command(subcommand)]
        channel: SendChannel,
    },

    #[command(about = "调试开关", long_about = "动态开启/关闭/查看实例调试状态。")]
    Debug {
        #[command(subcommand)]
        action: DebugAction,
    },

    #[command(hide = true)]
    NotifyOrchestrator {
        #[arg(long, default_value = "default")]
        instance: String,

        #[arg(long)]
        orchestrator: String,

        #[arg(long)]
        req_id: String,

        #[arg(long)]
        kind: String,
    },

    #[command(hide = true)]
    PaneFeed {
        #[arg(long, default_value = "default")]
        instance: String,

        #[arg(long)]
        provider: String,
    },

    #[command(
        external_subcommand,
        about = "兼容快捷入口",
        long_about = "兼容旧习惯调用：\n- provider 启动：rccb claude codex ...\n- 别名命令：rccb cask/cping/cpend ..."
    )]
    External(Vec<String>),
}

#[derive(Subcommand, Debug)]
pub enum UpdateCommand {
    #[command(about = "检查当前平台是否有新版本")]
    Check {
        #[arg(long, default_value_t = false, help = "以 JSON 输出")]
        as_json: bool,
    },

    #[command(about = "下载、校验并安装最新版本")]
    Apply {
        #[arg(long, help = "指定目标版本，例如 v0.1.1；省略时安装最新版本")]
        version: Option<String>,

        #[arg(long, help = "安装到指定路径；省略时默认尝试覆盖当前可执行文件")]
        install_path: Option<PathBuf>,

        #[arg(long, default_value_t = false, help = "即使已是同版本也强制重新安装")]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum SendChannel {
    #[command(about = "发送飞书机器人消息")]
    Feishu {
        #[arg(long, help = "飞书机器人 webhook URL")]
        webhook_url: String,

        #[arg(long, help = "消息文本")]
        text: String,
    },

    #[command(about = "发送 Telegram 机器人消息")]
    Telegram {
        #[arg(long, help = "机器人 token")]
        bot_token: String,

        #[arg(long, help = "目标 chat_id")]
        chat_id: String,

        #[arg(long, help = "消息文本")]
        text: String,

        #[arg(long, help = "可选：话题线程 ID")]
        message_thread_id: Option<i64>,
    },
}

#[derive(Subcommand, Debug)]
pub enum DebugAction {
    #[command(about = "开启调试")]
    On {
        #[arg(long, default_value = "default", help = "实例 ID")]
        instance: String,
    },
    #[command(about = "关闭调试")]
    Off {
        #[arg(long, default_value = "default", help = "实例 ID")]
        instance: String,
    },
    #[command(about = "查看调试状态")]
    Status {
        #[arg(long, default_value = "default", help = "实例 ID")]
        instance: String,
    },
}

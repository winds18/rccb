use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "rccb", version, about = "Rust CCB with project-local bindings")]
pub struct Cli {
    #[arg(
        long,
        default_value = ".",
        help = "Project directory. rccb stores state in <project>/.rccb"
    )]
    pub project_dir: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    Init {
        #[arg(long, default_value_t = false)]
        force: bool,
    },

    Start {
        #[arg(long, default_value = "default")]
        instance: String,

        #[arg(long, default_value_t = 5)]
        heartbeat_secs: u64,

        #[arg(long, default_value = "127.0.0.1:0", help = "Daemon listen address")]
        listen: String,

        #[arg(long, help = "Optional initial task text written to .rccb/tasks")]
        task: Option<String>,

        #[arg(
            long,
            default_value_t = false,
            help = "Enable full debug logging for this instance"
        )]
        debug: bool,

        #[arg(value_name = "PROVIDER", num_args = 0.., help = "Provider launch order; first is orchestrator")]
        providers: Vec<String>,
    },

    Status {
        #[arg(long)]
        instance: Option<String>,

        #[arg(long, default_value_t = false)]
        as_json: bool,
    },

    Tasks {
        #[arg(long)]
        instance: Option<String>,

        #[arg(long, default_value_t = 20)]
        limit: usize,

        #[arg(long, default_value_t = false)]
        as_json: bool,
    },

    Stop {
        #[arg(long, default_value = "default")]
        instance: String,
    },

    Ping {
        #[arg(long, default_value = "default")]
        instance: String,

        #[arg(long, default_value_t = 1.0)]
        timeout_s: f64,
    },

    Cancel {
        #[arg(long, default_value = "default")]
        instance: String,

        #[arg(long, help = "Request id to cancel")]
        req_id: String,
    },

    Ask {
        #[arg(long, default_value = "default")]
        instance: String,

        #[arg(long)]
        provider: String,

        #[arg(long, default_value = "manual")]
        caller: String,

        #[arg(long, default_value_t = 300.0)]
        timeout_s: f64,

        #[arg(long, default_value_t = false)]
        quiet: bool,

        #[arg(long, default_value_t = false)]
        stream: bool,

        #[arg(long)]
        req_id: Option<String>,

        #[arg(value_name = "MESSAGE", num_args = 0.., help = "Message text; if empty read from stdin")]
        message: Vec<String>,
    },

    Send {
        #[command(subcommand)]
        channel: SendChannel,
    },

    Debug {
        #[command(subcommand)]
        action: DebugAction,
    },

    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(Subcommand, Debug)]
pub enum SendChannel {
    Feishu {
        #[arg(long)]
        webhook_url: String,

        #[arg(long)]
        text: String,
    },

    Telegram {
        #[arg(long)]
        bot_token: String,

        #[arg(long)]
        chat_id: String,

        #[arg(long)]
        text: String,

        #[arg(long)]
        message_thread_id: Option<i64>,
    },
}

#[derive(Subcommand, Debug)]
pub enum DebugAction {
    On {
        #[arg(long, default_value = "default")]
        instance: String,
    },
    Off {
        #[arg(long, default_value = "default")]
        instance: String,
    },
    Status {
        #[arg(long, default_value = "default")]
        instance: String,
    },
}

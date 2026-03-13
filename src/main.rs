mod cli;
mod commands;
mod completion_hook;
mod constants;
mod daemon;
mod im;
mod io_utils;
mod layout;
mod protocol;
mod provider;
mod types;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};
use crate::commands::{
    cmd_ask, cmd_cancel, cmd_debug, cmd_external_provider_launch, cmd_init, cmd_mounted, cmd_ping,
    cmd_send, cmd_start, cmd_status, cmd_stop, cmd_tasks, cmd_watch,
};
use crate::io_utils::resolve_project_dir;

fn main() -> Result<()> {
    let cli = Cli::parse();
    let project_dir = resolve_project_dir(&cli.project_dir)?;

    match cli.command {
        Command::Init { force } => cmd_init(&project_dir, force),
        Command::Start {
            instance,
            heartbeat_secs,
            listen,
            task,
            debug,
            providers,
        } => cmd_start(
            &project_dir,
            &instance,
            heartbeat_secs,
            &listen,
            providers,
            task,
            debug,
        ),
        Command::Status { instance, as_json } => {
            cmd_status(&project_dir, instance.as_deref(), as_json)
        }
        Command::Mounted { instance, as_json } => {
            cmd_mounted(&project_dir, instance.as_deref(), as_json)
        }
        Command::Tasks {
            instance,
            limit,
            as_json,
        } => cmd_tasks(&project_dir, instance.as_deref(), limit, as_json),
        Command::Watch {
            instance,
            req_id,
            provider,
            all,
            poll_ms,
            timeout_s,
            follow,
            with_provider_log,
            with_debug_log,
            pane_ui,
            as_json,
        } => cmd_watch(
            &project_dir,
            &instance,
            req_id.as_deref(),
            provider.as_deref(),
            all,
            poll_ms,
            timeout_s,
            follow,
            with_provider_log,
            with_debug_log,
            pane_ui,
            as_json,
        ),
        Command::Stop { instance } => cmd_stop(&project_dir, &instance),
        Command::Ping {
            instance,
            timeout_s,
        } => cmd_ping(&project_dir, &instance, timeout_s),
        Command::Cancel { instance, req_id } => cmd_cancel(&project_dir, &instance, &req_id),
        Command::Ask {
            instance,
            provider,
            caller,
            timeout_s,
            quiet,
            stream,
            async_submit,
            req_id,
            message,
        } => cmd_ask(
            &project_dir,
            &instance,
            &provider,
            &caller,
            timeout_s,
            quiet,
            stream,
            async_submit,
            req_id,
            message,
        ),
        Command::Send { channel } => cmd_send(channel),
        Command::Debug { action } => cmd_debug(&project_dir, action),
        Command::External(raw) => cmd_external_provider_launch(&project_dir, raw),
    }
}

mod cli;
mod commands;
mod completion_hook;
mod constants;
mod daemon;
mod im;
mod io_utils;
mod layout;
mod orchestrator_callback;
mod orchestrator_lock;
mod protocol;
mod provider;
mod types;
mod updater;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command, UpdateCommand};
use crate::commands::{
    cmd_ask, cmd_await, cmd_cancel, cmd_debug, cmd_external_provider_launch, cmd_inbox, cmd_init,
    cmd_mounted, cmd_ping, cmd_send, cmd_shortcut_restore, cmd_start, cmd_status, cmd_stop,
    cmd_tasks, cmd_watch,
};
use crate::io_utils::{cleanup_project_retention, resolve_project_dir};
use crate::orchestrator_callback::cmd_orchestrator_notify;
use crate::provider::cmd_pane_feed;
use crate::updater::{cmd_update_apply, cmd_update_check, maybe_auto_update_notice};

fn main() -> Result<()> {
    let cli = Cli::parse();
    let project_dir = resolve_project_dir(&cli.project_dir)?;
    if let Err(err) = cleanup_project_retention(&project_dir) {
        eprintln!("warn: skip .rccb retention cleanup: {}", err);
    }
    let defer_auto_update_notice = matches!(
        cli.command.as_ref(),
        None | Some(Command::Start { .. }) | Some(Command::External(_))
    );
    if !defer_auto_update_notice {
        maybe_auto_update_notice(&project_dir, cli.command.as_ref());
    }

    match cli.command {
        None => cmd_shortcut_restore(&project_dir),
        Some(Command::Init { force }) => cmd_init(&project_dir, force),
        Some(Command::Start {
            instance,
            heartbeat_secs,
            listen,
            task,
            debug,
            providers,
        }) => cmd_start(
            &project_dir,
            &instance,
            heartbeat_secs,
            &listen,
            providers,
            task,
            debug,
        ),
        Some(Command::Status { instance, as_json }) => {
            cmd_status(&project_dir, instance.as_deref(), as_json)
        }
        Some(Command::Mounted { instance, as_json }) => {
            cmd_mounted(&project_dir, instance.as_deref(), as_json)
        }
        Some(Command::Tasks {
            instance,
            limit,
            as_json,
        }) => cmd_tasks(&project_dir, instance.as_deref(), limit, as_json),
        Some(Command::Inbox {
            instance,
            orchestrator,
            req_id,
            kind,
            latest,
            limit,
            as_json,
        }) => cmd_inbox(
            &project_dir,
            &instance,
            orchestrator.as_deref(),
            req_id.as_deref(),
            kind.as_deref(),
            latest,
            limit,
            as_json,
        ),
        Some(Command::Watch {
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
        }) => cmd_watch(
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
        Some(Command::Stop { instance }) => cmd_stop(&project_dir, &instance),
        Some(Command::Ping {
            instance,
            timeout_s,
        }) => cmd_ping(&project_dir, &instance, timeout_s),
        Some(Command::Cancel { instance, req_id }) => cmd_cancel(&project_dir, &instance, &req_id),
        Some(Command::Ask {
            instance,
            provider,
            caller,
            timeout_s,
            quiet,
            stream,
            async_submit,
            await_terminal,
            req_id,
            message,
        }) => cmd_ask(
            &project_dir,
            &instance,
            &provider,
            &caller,
            timeout_s,
            quiet,
            stream,
            async_submit,
            await_terminal,
            req_id,
            message,
        ),
        Some(Command::Await {
            instance,
            req_id,
            timeout_s,
            as_json,
        }) => cmd_await(&project_dir, &instance, &req_id, timeout_s, as_json),
        Some(Command::Send { channel }) => cmd_send(channel),
        Some(Command::Debug { action }) => cmd_debug(&project_dir, action),
        Some(Command::Update { action }) => match action {
            UpdateCommand::Check { as_json } => cmd_update_check(&project_dir, as_json),
            UpdateCommand::Apply {
                version,
                install_path,
                force,
            } => cmd_update_apply(
                &project_dir,
                version.as_deref(),
                install_path.as_deref(),
                force,
            ),
        },
        Some(Command::NotifyOrchestrator {
            instance,
            orchestrator,
            req_id,
            kind,
        }) => cmd_orchestrator_notify(&project_dir, &instance, &orchestrator, &req_id, &kind),
        Some(Command::PaneFeed { instance, provider }) => {
            cmd_pane_feed(&project_dir, &instance, &provider)
        }
        Some(Command::External(raw)) => cmd_external_provider_launch(&project_dir, raw),
    }
}

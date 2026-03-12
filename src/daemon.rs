use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use fs2::FileExt;
use serde::Deserialize;
use serde_json::{json, Value};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::flag;

use crate::completion_hook::{notify_completion_async, CompletionHookInput};
use crate::constants::{PROTOCOL_PREFIX, PROTOCOL_VERSION, SUPPORTED_PROVIDERS};
use crate::io_utils::{
    make_req_id, normalize_connect_host, now_unix, now_unix_ms, parse_listen_addr, random_token,
    update_task_status, write_json_pretty, write_line, write_state,
};
use crate::layout::{
    ensure_project_layout, launcher_feed_path, launcher_meta_path, lock_path, logs_instance_dir,
    sanitize_filename, sanitize_instance, session_instance_dir, state_path, tasks_instance_dir,
    tmp_instance_dir,
};
use crate::protocol::{write_json_event_line, write_json_line, write_json_value_line};
use crate::provider::execute_provider_request;
use crate::types::{
    AskEvent, AskRequest, AskResponse, DaemonContext, InstanceState, OrchestrationArtifacts,
    OrchestrationPlan, PendingStream, WorkerEvent, WorkerTask,
};

pub fn start_instance(
    project_dir: &Path,
    instance: &str,
    heartbeat_secs: u64,
    listen: &str,
    providers: Vec<String>,
    initial_task: Option<String>,
    debug_enabled: bool,
) -> Result<()> {
    if heartbeat_secs == 0 {
        bail!("heartbeat_secs must be > 0");
    }

    ensure_project_layout(project_dir)?;

    let lock_path = lock_path(project_dir, instance);
    let state_path = state_path(project_dir, instance);

    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("open lock file failed: {}", lock_path.display()))?;

    if let Err(err) = lock_file.try_lock_exclusive() {
        bail!(
            "instance already running or locked. project={} instance={} lock={} err={}",
            project_dir.display(),
            instance,
            lock_path.display(),
            err
        );
    }

    let (host, port) = parse_listen_addr(listen)?;
    let listener = TcpListener::bind((host.as_str(), port))
        .with_context(|| format!("bind listen failed: {}", listen))?;
    listener
        .set_nonblocking(true)
        .context("set nonblocking listener failed")?;
    let actual_addr = listener.local_addr().context("read local_addr failed")?;
    let actual_host = normalize_connect_host(&actual_addr.ip().to_string());
    let actual_port = actual_addr.port();

    let normalized = if providers.is_empty() {
        SUPPORTED_PROVIDERS.iter().map(|x| x.to_string()).collect()
    } else {
        providers
    };
    let plan = build_orchestration_plan(&normalized)?;

    let pid = std::process::id();
    let now = now_unix();
    let token = random_token();

    let mut state = InstanceState {
        schema_version: 1,
        instance_id: sanitize_instance(instance),
        project_dir: project_dir.display().to_string(),
        pid,
        status: "running".to_string(),
        started_at_unix: now,
        last_heartbeat_unix: now,
        stopped_at_unix: None,
        providers: plan.providers.clone(),
        orchestrator: Some(plan.orchestrator.clone()),
        executors: plan.executors.clone(),
        session_file: None,
        last_task_id: None,
        daemon_host: Some(actual_host.clone()),
        daemon_port: Some(actual_port),
        daemon_token: Some(token.clone()),
        debug_enabled,
    };

    let artifacts = write_orchestration_records(
        project_dir,
        &state.instance_id,
        &plan,
        initial_task.as_deref(),
        pid,
    )?;
    state.session_file = Some(artifacts.session_file.display().to_string());
    state.last_task_id = Some(artifacts.task_id.clone());

    write_state(&state_path, &state)?;

    let shutdown = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&shutdown))?;
    flag::register(SIGTERM, Arc::clone(&shutdown))?;

    let shared_state = Arc::new(Mutex::new(state));

    let context = DaemonContext {
        project_dir: project_dir.to_path_buf(),
        instance_id: sanitize_instance(instance),
        state_path: state_path.clone(),
        shared_state: Arc::clone(&shared_state),
        allowed_providers: plan.providers.clone(),
    };

    if debug_enabled {
        debug_log(
            &context,
            &format!(
                "[DEBUG] debug mode enabled instance={} log_file={}",
                context.instance_id,
                debug_log_path(&context).display()
            ),
        );
    }

    let pool = Arc::new(WorkerPool::new(context.clone()));

    println!(
        "orchestration: instance={} orchestrator={} executors={} session={} task={}",
        context.instance_id,
        plan.orchestrator,
        if plan.executors.is_empty() {
            "-".to_string()
        } else {
            plan.executors.join(",")
        },
        artifacts.session_file.display(),
        artifacts.task_file.display(),
    );

    println!(
        "rccb started: project={} instance={} pid={} listen={}:{} state={}",
        project_dir.display(),
        context.instance_id,
        pid,
        actual_host,
        actual_port,
        state_path.display(),
    );

    let mut last_heartbeat = Instant::now();

    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let context_cloned = context.clone();
                let pool_cloned = Arc::clone(&pool);
                let token_cloned = token.clone();
                let shutdown_cloned = Arc::clone(&shutdown);

                thread::spawn(move || {
                    if let Err(err) = handle_connection(
                        stream,
                        &context_cloned,
                        &pool_cloned,
                        &token_cloned,
                        &shutdown_cloned,
                    ) {
                        let _ = write_line(
                            logs_instance_dir(
                                &context_cloned.project_dir,
                                &context_cloned.instance_id,
                            )
                            .join("daemon.log"),
                            &format!("[ERROR] connection handler failed: {}", err),
                        );
                    }
                });
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => {
                let _ = write_line(
                    logs_instance_dir(project_dir, &context.instance_id).join("daemon.log"),
                    &format!("[ERROR] accept failed: {}", err),
                );
                thread::sleep(Duration::from_millis(100));
            }
        }

        if last_heartbeat.elapsed() >= Duration::from_secs(heartbeat_secs) {
            update_heartbeat(&context)?;
            last_heartbeat = Instant::now();
        }
    }

    {
        let mut guard = context
            .shared_state
            .lock()
            .map_err(|_| anyhow!("state lock poisoned while stopping"))?;
        guard.status = "stopped".to_string();
        guard.stopped_at_unix = Some(now_unix());
        guard.last_heartbeat_unix = guard.stopped_at_unix.unwrap_or(guard.last_heartbeat_unix);
        write_state(&context.state_path, &guard)?;
    }

    lock_file.unlock()?;
    println!("rccb stopped: instance={} pid={}", context.instance_id, pid);
    Ok(())
}

fn handle_connection(
    mut stream: TcpStream,
    context: &DaemonContext,
    pool: &Arc<WorkerPool>,
    token: &str,
    shutdown: &Arc<AtomicBool>,
) -> Result<()> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .context("set read timeout failed")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .context("set write timeout failed")?;

    let mut reader = BufReader::new(stream.try_clone().context("clone stream failed")?);
    let mut line = String::new();
    let n = reader.read_line(&mut line).context("read line failed")?;
    if n == 0 {
        return Ok(());
    }

    let value: Value = serde_json::from_str(&line).context("invalid json line")?;
    debug_wire_in(context, &value);

    let recv_token = value
        .get("token")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let request_id = value
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    if recv_token != token {
        let unauthorized = AskResponse::unauthorized(request_id);
        debug_log_json(context, "[WIRE][OUT][unauthorized]", &unauthorized);
        write_json_line(&mut stream, &unauthorized)?;
        return Ok(());
    }

    let msg_type = value
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    match msg_type {
        "ask.ping" => {
            let resp = json!({
                "type": "ask.pong",
                "v": PROTOCOL_VERSION,
                "id": request_id,
                "exit_code": 0,
                "reply": "OK"
            });
            debug_wire_out_value(context, &resp);
            write_json_value_line(&mut stream, &resp)?;
            Ok(())
        }
        "ask.shutdown" => {
            let resp = AskResponse {
                msg_type: format!("{}.response", PROTOCOL_PREFIX),
                v: PROTOCOL_VERSION,
                id: request_id,
                req_id: None,
                exit_code: 0,
                reply: "OK".to_string(),
                provider: None,
                meta: None,
            };
            debug_wire_out_response(context, &resp);
            write_json_line(&mut stream, &resp)?;
            shutdown.store(true, Ordering::Relaxed);
            Ok(())
        }
        "ask.debug" => {
            let action = value
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("status")
                .trim()
                .to_ascii_lowercase();
            let mut guard = context
                .shared_state
                .lock()
                .map_err(|_| anyhow!("state lock poisoned after ask.debug"))?;

            match action.as_str() {
                "on" => guard.debug_enabled = true,
                "off" => guard.debug_enabled = false,
                "status" => {}
                other => {
                    let resp = AskResponse {
                        msg_type: format!("{}.response", PROTOCOL_PREFIX),
                        v: PROTOCOL_VERSION,
                        id: request_id,
                        req_id: None,
                        exit_code: 1,
                        reply: format!("invalid debug action `{}`", other),
                        provider: None,
                        meta: Some(json!({"status":"bad_request"})),
                    };
                    debug_wire_out_response(context, &resp);
                    write_json_line(&mut stream, &resp)?;
                    return Ok(());
                }
            }

            write_state(&context.state_path, &guard)?;
            let enabled = guard.debug_enabled;
            drop(guard);

            let resp = AskResponse {
                msg_type: format!("{}.response", PROTOCOL_PREFIX),
                v: PROTOCOL_VERSION,
                id: request_id,
                req_id: None,
                exit_code: 0,
                reply: format!("debug {}", if enabled { "enabled" } else { "disabled" }),
                provider: None,
                meta: Some(json!({
                    "debug_enabled": enabled,
                    "debug_log_path": debug_log_path(context).display().to_string(),
                })),
            };

            debug_wire_out_response(context, &resp);
            write_json_line(&mut stream, &resp)?;
            Ok(())
        }
        "ask.cancel" => {
            let target_req_id = value
                .get("req_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            if target_req_id.is_empty() {
                let resp = AskResponse {
                    msg_type: format!("{}.response", PROTOCOL_PREFIX),
                    v: PROTOCOL_VERSION,
                    id: request_id,
                    req_id: None,
                    exit_code: 1,
                    reply: "Bad request: req_id is required".to_string(),
                    provider: None,
                    meta: Some(json!({"status":"bad_request"})),
                };
                debug_wire_out_response(context, &resp);
                write_json_line(&mut stream, &resp)?;
                return Ok(());
            }

            let found = pool.cancel(&target_req_id);
            let resp = AskResponse {
                msg_type: format!("{}.response", PROTOCOL_PREFIX),
                v: PROTOCOL_VERSION,
                id: request_id,
                req_id: Some(target_req_id.clone()),
                exit_code: if found { 0 } else { 1 },
                reply: if found {
                    "cancel signal submitted".to_string()
                } else {
                    "request not found or already finished".to_string()
                },
                provider: None,
                meta: Some(json!({
                    "status": if found { "cancel_requested" } else { "not_found" }
                })),
            };
            debug_wire_out_response(context, &resp);
            write_json_line(&mut stream, &resp)?;
            Ok(())
        }
        "ask.request" => {
            let req: AskRequest = match serde_json::from_value(value) {
                Ok(v) => v,
                Err(err) => {
                    let resp = AskResponse {
                        msg_type: format!("{}.response", PROTOCOL_PREFIX),
                        v: PROTOCOL_VERSION,
                        id: request_id,
                        req_id: None,
                        exit_code: 1,
                        reply: format!("Bad request: {}", err),
                        provider: None,
                        meta: Some(json!({"status": "bad_request"})),
                    };
                    debug_wire_out_response(context, &resp);
                    write_json_line(&mut stream, &resp)?;
                    return Ok(());
                }
            };
            debug_log_json(context, "[REQUEST]", &req);
            let req_id = req.req_id.clone().unwrap_or_else(make_req_id);
            let task_file = write_request_task(context, &req, &req_id)?;
            relay_task_dispatched(context, &req, &req_id);

            {
                let mut guard = context
                    .shared_state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned after ask.request"))?;
                guard.last_task_id = Some(req_id.clone());
                write_state(&context.state_path, &guard)?;
            }

            if req.stream && req.async_mode {
                let resp = AskResponse {
                    msg_type: format!("{}.response", PROTOCOL_PREFIX),
                    v: PROTOCOL_VERSION,
                    id: request_id,
                    req_id: Some(req_id),
                    exit_code: 1,
                    reply: "stream and async are mutually exclusive".to_string(),
                    provider: Some(req.provider),
                    meta: Some(json!({"status":"bad_request"})),
                };
                debug_wire_out_response(context, &resp);
                write_json_line(&mut stream, &resp)
            } else if req.async_mode {
                if let Err(err) = pool.submit_async(req.clone(), req_id.clone(), task_file) {
                    let resp = AskResponse {
                        msg_type: format!("{}.response", PROTOCOL_PREFIX),
                        v: PROTOCOL_VERSION,
                        id: request_id,
                        req_id: Some(req_id),
                        exit_code: 1,
                        reply: format!("enqueue failed: {}", err),
                        provider: Some(req.provider),
                        meta: Some(json!({"status": "failed"})),
                    };
                    debug_wire_out_response(context, &resp);
                    return write_json_line(&mut stream, &resp);
                }
                let resp = AskResponse {
                    msg_type: format!("{}.response", PROTOCOL_PREFIX),
                    v: PROTOCOL_VERSION,
                    id: request_id,
                    req_id: Some(req_id),
                    exit_code: 0,
                    reply: "submitted".to_string(),
                    provider: Some(req.provider),
                    meta: Some(json!({"status": "queued"})),
                };
                debug_wire_out_response(context, &resp);
                write_json_line(&mut stream, &resp)
            } else if req.stream {
                let pending = pool.submit_stream(req, req_id.clone(), task_file)?;
                forward_stream_events(&mut stream, context, &request_id, pending)
            } else {
                let mut response = pool.submit(req, req_id.clone(), task_file)?;
                response.id = request_id;
                debug_wire_out_response(context, &response);
                write_json_line(&mut stream, &response)
            }
        }
        _ => {
            let resp = AskResponse {
                msg_type: format!("{}.response", PROTOCOL_PREFIX),
                v: PROTOCOL_VERSION,
                id: request_id,
                req_id: None,
                exit_code: 1,
                reply: format!("Invalid request type: {}", msg_type),
                provider: None,
                meta: None,
            };
            debug_wire_out_response(context, &resp);
            write_json_line(&mut stream, &resp)
        }
    }
}

fn forward_stream_events(
    stream: &mut TcpStream,
    context: &DaemonContext,
    request_id: &str,
    pending: PendingStream,
) -> Result<()> {
    let timeout = if pending.timeout_s < 0.0 {
        None
    } else {
        Some(Duration::from_secs_f64(pending.timeout_s + 5.0))
    };
    let start = Instant::now();
    let mut done = false;

    while !done {
        if let Some(limit) = timeout {
            if start.elapsed() >= limit {
                let event = AskEvent::error(
                    request_id.to_string(),
                    None,
                    None,
                    "request timeout while streaming".to_string(),
                );
                debug_wire_out_event(context, &event);
                write_json_event_line(stream, &event)?;
                break;
            }
        }

        let wait = Duration::from_millis(200);
        match pending.event_rx.recv_timeout(wait) {
            Ok(WorkerEvent::Start {
                provider,
                req_id,
                meta,
            }) => {
                let evt = AskEvent::start(request_id.to_string(), req_id, provider, meta);
                debug_wire_out_event(context, &evt);
                write_json_event_line(stream, &evt)?;
            }
            Ok(WorkerEvent::Delta {
                provider,
                req_id,
                delta,
            }) => {
                let evt = AskEvent::delta(request_id.to_string(), req_id, provider, delta);
                debug_wire_out_event(context, &evt);
                write_json_event_line(stream, &evt)?;
            }
            Ok(WorkerEvent::Done { response }) => {
                let evt = AskEvent::done(request_id.to_string(), response);
                debug_wire_out_event(context, &evt);
                write_json_event_line(stream, &evt)?;
                done = true;
            }
            Ok(WorkerEvent::Error {
                provider,
                req_id,
                message,
            }) => {
                let evt = AskEvent::error(
                    request_id.to_string(),
                    Some(req_id),
                    Some(provider),
                    message,
                );
                debug_wire_out_event(context, &evt);
                write_json_event_line(stream, &evt)?;
                done = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let evt = AskEvent::error(
                    request_id.to_string(),
                    None,
                    None,
                    "stream channel closed unexpectedly".to_string(),
                );
                debug_wire_out_event(context, &evt);
                write_json_event_line(stream, &evt)?;
                done = true;
            }
        }
    }

    Ok(())
}

struct WorkerPool {
    context: DaemonContext,
    workers: Mutex<HashMap<String, mpsc::Sender<WorkerTask>>>,
    cancel_flags: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

impl WorkerPool {
    fn new(context: DaemonContext) -> Self {
        Self {
            context,
            workers: Mutex::new(HashMap::new()),
            cancel_flags: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn submit(
        &self,
        request: AskRequest,
        req_id: String,
        task_file: PathBuf,
    ) -> Result<AskResponse> {
        if !self.is_provider_enabled(&request.provider) {
            return Ok(rejected_response(
                &self.context.instance_id,
                request,
                req_id,
            ));
        }

        let sender = self.get_worker_sender(&request.provider)?;

        let (resp_tx, resp_rx) = mpsc::channel::<AskResponse>();
        let timeout = if request.timeout_s < 0.0 {
            Duration::from_secs(24 * 3600)
        } else {
            Duration::from_secs_f64(request.timeout_s + 5.0)
        };
        let cancel_flag = self.register_cancel_flag(&req_id)?;

        if let Err(err) = sender.send(WorkerTask {
            request,
            req_id: req_id.clone(),
            task_file,
            cancel_flag,
            response_tx: Some(resp_tx),
            stream_tx: None,
        }) {
            self.remove_cancel_flag(&req_id);
            return Err(anyhow!("enqueue worker task failed: {}", err));
        }

        match resp_rx.recv_timeout(timeout) {
            Ok(resp) => Ok(resp),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(AskResponse {
                msg_type: format!("{}.response", PROTOCOL_PREFIX),
                v: PROTOCOL_VERSION,
                id: "".to_string(),
                req_id: Some(req_id),
                exit_code: 2,
                reply: "request timeout".to_string(),
                provider: None,
                meta: Some(json!({"status": "timeout"})),
            }),
            Err(err) => Err(anyhow!("worker response channel error: {}", err)),
        }
    }

    fn submit_stream(
        &self,
        request: AskRequest,
        req_id: String,
        task_file: PathBuf,
    ) -> Result<PendingStream> {
        if !self.is_provider_enabled(&request.provider) {
            let (tx, rx) = mpsc::channel();
            let _ = tx.send(WorkerEvent::Error {
                provider: request.provider,
                req_id,
                message: format!(
                    "provider not enabled for instance `{}`",
                    self.context.instance_id
                ),
            });
            return Ok(PendingStream {
                event_rx: rx,
                timeout_s: 1.0,
            });
        }

        let timeout_s = request.timeout_s;
        let sender = self.get_worker_sender(&request.provider)?;
        let (event_tx, event_rx) = mpsc::channel::<WorkerEvent>();
        let cancel_flag = self.register_cancel_flag(&req_id)?;

        if let Err(err) = sender.send(WorkerTask {
            request,
            req_id: req_id.clone(),
            task_file,
            cancel_flag,
            response_tx: None,
            stream_tx: Some(event_tx),
        }) {
            self.remove_cancel_flag(&req_id);
            return Err(anyhow!("enqueue streaming worker task failed: {}", err));
        }

        Ok(PendingStream {
            event_rx,
            timeout_s,
        })
    }

    fn submit_async(&self, request: AskRequest, req_id: String, task_file: PathBuf) -> Result<()> {
        if !self.is_provider_enabled(&request.provider) {
            return Err(anyhow!(
                "provider `{}` not enabled for instance `{}`",
                request.provider,
                self.context.instance_id
            ));
        }

        let sender = self.get_worker_sender(&request.provider)?;
        let cancel_flag = self.register_cancel_flag(&req_id)?;
        if let Err(err) = sender.send(WorkerTask {
            request,
            req_id: req_id.clone(),
            task_file,
            cancel_flag,
            response_tx: None,
            stream_tx: None,
        }) {
            self.remove_cancel_flag(&req_id);
            return Err(anyhow!("enqueue async worker task failed: {}", err));
        }
        Ok(())
    }

    fn is_provider_enabled(&self, provider: &str) -> bool {
        self.context.allowed_providers.iter().any(|x| x == provider)
    }

    fn get_worker_sender(&self, provider: &str) -> Result<mpsc::Sender<WorkerTask>> {
        let provider_key = provider.to_string();
        let worker_key = format!("{}:{}", provider_key, self.context.instance_id);

        let sender = {
            let mut guard = self
                .workers
                .lock()
                .map_err(|_| anyhow!("worker map lock poisoned"))?;

            if let Some(tx) = guard.get(&worker_key) {
                tx.clone()
            } else {
                let (tx, rx) = mpsc::channel::<WorkerTask>();
                let context_cloned = self.context.clone();
                let key_cloned = worker_key.clone();
                let cancel_flags_cloned = Arc::clone(&self.cancel_flags);
                thread::spawn(move || {
                    worker_loop(key_cloned, context_cloned, rx, cancel_flags_cloned)
                });
                guard.insert(worker_key, tx.clone());
                tx
            }
        };

        Ok(sender)
    }

    fn register_cancel_flag(&self, req_id: &str) -> Result<Arc<AtomicBool>> {
        let mut guard = self
            .cancel_flags
            .lock()
            .map_err(|_| anyhow!("cancel flag map lock poisoned"))?;
        let flag = Arc::new(AtomicBool::new(false));
        guard.insert(req_id.to_string(), Arc::clone(&flag));
        Ok(flag)
    }

    fn cancel(&self, req_id: &str) -> bool {
        let guard = match self.cancel_flags.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        if let Some(flag) = guard.get(req_id) {
            flag.store(true, Ordering::Relaxed);
            return true;
        }
        false
    }

    fn remove_cancel_flag(&self, req_id: &str) {
        if let Ok(mut guard) = self.cancel_flags.lock() {
            guard.remove(req_id);
        }
    }
}

fn rejected_response(instance_id: &str, request: AskRequest, req_id: String) -> AskResponse {
    AskResponse {
        msg_type: format!("{}.response", PROTOCOL_PREFIX),
        v: PROTOCOL_VERSION,
        id: request.id,
        req_id: Some(req_id),
        exit_code: 1,
        reply: format!(
            "provider `{}` not enabled for instance `{}`",
            request.provider, instance_id
        ),
        provider: Some(request.provider),
        meta: Some(json!({"status": "rejected"})),
    }
}

fn worker_loop(
    worker_key: String,
    context: DaemonContext,
    rx: mpsc::Receiver<WorkerTask>,
    cancel_flags: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
) {
    let log_file = logs_instance_dir(&context.project_dir, &context.instance_id).join("daemon.log");
    let _ = write_line(
        log_file.clone(),
        &format!("[INFO] worker started key={}", worker_key),
    );

    for task in rx {
        let started_at = now_unix();
        let req = task.request.clone();
        debug_log_json(&context, "[WORKER][TASK][queued]", &req);

        let _ = update_task_status(
            &task.task_file,
            "running",
            Some(started_at),
            None,
            None,
            None,
        );

        let provider_log = logs_instance_dir(&context.project_dir, &context.instance_id)
            .join(format!("{}.log", req.provider));

        let _ = write_line(
            provider_log.clone(),
            &format!(
                "[INFO] req_id={} caller={} provider={} worker={} msg_len={} stream={}",
                task.req_id,
                req.caller,
                req.provider,
                worker_key,
                req.message.len(),
                req.stream
            ),
        );

        let role = if context
            .shared_state
            .lock()
            .ok()
            .and_then(|s| s.orchestrator.clone())
            .unwrap_or_default()
            == req.provider
        {
            "orchestrator"
        } else {
            "executor"
        };

        if let Some(tx) = &task.stream_tx {
            debug_log(
                &context,
                &format!(
                    "[WORKER][STREAM][start] req_id={} provider={} worker={}",
                    task.req_id, req.provider, worker_key
                ),
            );
            let _ = tx.send(WorkerEvent::Start {
                provider: req.provider.clone(),
                req_id: task.req_id.clone(),
                meta: Some(json!({
                    "status": "running",
                    "worker": worker_key,
                    "role": role,
                })),
            });
        }

        let req_id_for_stream = task.req_id.clone();
        let provider_for_stream = req.provider.clone();
        let cancel_flag = Arc::clone(&task.cancel_flag);
        let mut stream_delta_idx = 0usize;
        let exec = execute_provider_request(
            &req,
            &task.req_id,
            |chunk| {
                if chunk.is_empty() {
                    return;
                }
                append_provider_stream_chunk(&provider_log, &task.req_id, &chunk);
                debug_log(
                    &context,
                    &format!(
                        "[WORKER][STREAM][delta] req_id={} provider={} idx={} chars={}",
                        req_id_for_stream,
                        provider_for_stream,
                        stream_delta_idx,
                        chunk.chars().count()
                    ),
                );
                stream_delta_idx += 1;
                if let Some(tx) = &task.stream_tx {
                    let _ = tx.send(WorkerEvent::Delta {
                        provider: provider_for_stream.clone(),
                        req_id: req_id_for_stream.clone(),
                        delta: chunk,
                    });
                }
            },
            || cancel_flag.load(Ordering::Relaxed),
        );

        let done_at = now_unix();
        let elapsed_ms = now_unix_ms().saturating_sub(started_at.saturating_mul(1000));
        let exec = match exec {
            Ok(v) => v,
            Err(err) => {
                debug_log(
                    &context,
                    &format!(
                        "[WORKER][ERROR] req_id={} provider={} err={}",
                        task.req_id, req.provider, err
                    ),
                );
                crate::provider::ProviderExecResult {
                    exit_code: 1,
                    reply: format!("provider execution failed: {}", err),
                    done_seen: false,
                    done_ms: None,
                    anchor_seen: false,
                    anchor_ms: None,
                    fallback_scan: false,
                    status: "failed".to_string(),
                    stderr: String::new(),
                    effective_timeout_s: req.timeout_s,
                    effective_quiet: req.quiet,
                }
            }
        };
        let reply = exec.reply.clone();
        let task_status = match exec.status.as_str() {
            "completed" => "completed",
            "timeout" => "timeout",
            "canceled" => "canceled",
            "incomplete" => "incomplete",
            _ => "failed",
        };

        let _ = update_task_status(
            &task.task_file,
            task_status,
            Some(started_at),
            Some(done_at),
            Some(exec.exit_code),
            Some(&reply),
        );
        let reply_for_debug = reply.clone();
        relay_task_completed(
            &context,
            &req,
            &task.req_id,
            task_status,
            exec.exit_code,
            &reply,
        );

        let resp = AskResponse {
            msg_type: format!("{}.response", PROTOCOL_PREFIX),
            v: PROTOCOL_VERSION,
            id: req.id,
            req_id: Some(task.req_id.clone()),
            exit_code: exec.exit_code,
            reply,
            provider: Some(req.provider.clone()),
            meta: Some(json!({
                "session_key": worker_key,
                "role": role,
                "status": exec.status,
                "done_seen": exec.done_seen,
                "done_ms": exec.done_ms.unwrap_or(elapsed_ms),
                "anchor_seen": exec.anchor_seen,
                "anchor_ms": exec.anchor_ms.unwrap_or(0),
                "fallback_scan": exec.fallback_scan,
                "effective_timeout_s": exec.effective_timeout_s,
                "effective_quiet": exec.effective_quiet,
                "log_path": provider_log.display().to_string(),
                "stderr": exec.stderr,
            })),
        };

        notify_completion_async(CompletionHookInput {
            provider: req.provider.clone(),
            caller: req.caller.clone(),
            req_id: task.req_id.clone(),
            status: exec.status.clone(),
            done_seen: exec.done_seen,
            exit_code: exec.exit_code,
            reply: resp.reply.clone(),
            instance_id: context.instance_id.clone(),
            project_dir: context.project_dir.display().to_string(),
            work_dir: req.work_dir.clone(),
            log_file: log_file.clone(),
        });

        if let Some(tx) = task.response_tx {
            let _ = tx.send(resp.clone());
        }

        if let Some(tx) = task.stream_tx {
            debug_log(
                &context,
                &format!(
                    "[WORKER][STREAM][done] req_id={} provider={} exit_code={}",
                    task.req_id, req.provider, resp.exit_code
                ),
            );
            let _ = tx.send(WorkerEvent::Done { response: resp });
        }

        debug_log_json(
            &context,
            "[WORKER][TASK][done]",
            &json!({
                "req_id": task.req_id.clone(),
                "provider": req.provider,
                "reply": reply_for_debug,
                "elapsed_ms": elapsed_ms
            }),
        );

        if let Ok(mut guard) = cancel_flags.lock() {
            guard.remove(&task.req_id);
        }
    }

    let _ = write_line(
        log_file,
        &format!("[INFO] worker stopped key={}", worker_key),
    );
}

fn append_provider_stream_chunk(provider_log: &Path, req_id: &str, chunk: &str) {
    let normalized = chunk.replace('\r', "");
    let mut emitted = false;
    for line in normalized.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        emitted = true;
        let _ = write_line(
            provider_log.to_path_buf(),
            &format!("[STREAM] req_id={} {}", req_id, line),
        );
    }

    if !emitted {
        let tail = normalized.trim();
        if !tail.is_empty() {
            let _ = write_line(
                provider_log.to_path_buf(),
                &format!("[STREAM] req_id={} {}", req_id, tail),
            );
        }
    }
}

fn build_orchestration_plan(providers: &[String]) -> Result<OrchestrationPlan> {
    if providers.is_empty() {
        bail!("at least one provider is required to build orchestration plan");
    }

    let orchestrator = providers[0].clone();
    let executors = if providers.len() > 1 {
        providers[1..].to_vec()
    } else {
        Vec::new()
    };

    Ok(OrchestrationPlan {
        providers: providers.to_vec(),
        orchestrator,
        executors,
    })
}

fn write_orchestration_records(
    project_dir: &Path,
    instance: &str,
    plan: &OrchestrationPlan,
    initial_task: Option<&str>,
    runner_pid: u32,
) -> Result<OrchestrationArtifacts> {
    let now = now_unix();

    let session_dir = session_instance_dir(project_dir, instance);
    let providers_dir = session_dir.join("providers");
    let tasks_dir = tasks_instance_dir(project_dir, instance);
    let tmp_dir = tmp_instance_dir(project_dir, instance);
    let logs_dir = logs_instance_dir(project_dir, instance);

    fs::create_dir_all(&session_dir)?;
    fs::create_dir_all(&providers_dir)?;
    fs::create_dir_all(&tasks_dir)?;
    fs::create_dir_all(&tmp_dir)?;
    fs::create_dir_all(&logs_dir)?;

    let session_id = format!("{}-{}", instance, now);
    let task_id = format!("task-{}", now);

    let provider_entries: Vec<Value> = plan
        .providers
        .iter()
        .map(|provider| {
            let role = if provider == &plan.orchestrator {
                "orchestrator"
            } else {
                "executor"
            };
            json!({
                "provider": provider,
                "role": role,
                "runtime": {
                    "session_file": providers_dir.join(format!("{}.json", provider)).display().to_string(),
                    "log_file": logs_dir.join(format!("{}.log", provider)).display().to_string(),
                    "tmp_dir": tmp_dir.join(provider).display().to_string()
                }
            })
        })
        .collect();

    let session_file = session_dir.join("session.json");
    let session_json = json!({
        "schema_version": 1,
        "session_id": session_id,
        "instance_id": instance,
        "project_dir": project_dir.display().to_string(),
        "created_at_unix": now,
        "runner_pid": runner_pid,
        "orchestration": {
            "orchestrator": plan.orchestrator,
            "executors": plan.executors,
            "providers": provider_entries
        },
        "paths": {
            "session_dir": session_dir.display().to_string(),
            "tasks_dir": tasks_dir.display().to_string(),
            "tmp_dir": tmp_dir.display().to_string(),
            "logs_dir": logs_dir.display().to_string()
        }
    });
    write_json_pretty(&session_file, &session_json)?;

    for provider in &plan.providers {
        let role = if provider == &plan.orchestrator {
            "orchestrator"
        } else {
            "executor"
        };

        let provider_state_file = providers_dir.join(format!("{}.json", provider));
        let provider_state = json!({
            "schema_version": 1,
            "instance_id": instance,
            "provider": provider,
            "role": role,
            "orchestrator": plan.orchestrator,
            "executors": plan.executors,
            "project_dir": project_dir.display().to_string(),
            "session_file": session_file.display().to_string(),
            "log_file": logs_dir.join(format!("{}.log", provider)).display().to_string(),
            "tmp_dir": tmp_dir.join(provider).display().to_string(),
            "created_at_unix": now
        });
        write_json_pretty(&provider_state_file, &provider_state)?;
        fs::create_dir_all(tmp_dir.join(provider))?;
    }

    let task_file = tasks_dir.join(format!("{}.json", task_id));
    let task_json = json!({
        "schema_version": 1,
        "task_id": task_id,
        "instance_id": instance,
        "project_dir": project_dir.display().to_string(),
        "created_at_unix": now,
        "status": "queued",
        "orchestrator": plan.orchestrator,
        "executors": plan.executors,
        "providers": plan.providers,
        "input": {
            "text": initial_task.unwrap_or(""),
            "source": if initial_task.is_some() { "cli" } else { "bootstrap" }
        },
        "artifacts": {
            "session_file": session_file.display().to_string(),
            "tmp_dir": tmp_dir.display().to_string(),
            "logs_dir": logs_dir.display().to_string()
        }
    });
    write_json_pretty(&task_file, &task_json)?;

    Ok(OrchestrationArtifacts {
        session_file,
        task_file,
        task_id,
    })
}

fn write_request_task(context: &DaemonContext, req: &AskRequest, req_id: &str) -> Result<PathBuf> {
    let task_id = format!("task-{}", sanitize_filename(req_id));
    let task_file = tasks_instance_dir(&context.project_dir, &context.instance_id)
        .join(format!("{}.json", task_id));

    let content = json!({
        "schema_version": 1,
        "task_id": task_id,
        "req_id": req_id,
        "instance_id": context.instance_id,
        "project_dir": context.project_dir.display().to_string(),
        "created_at_unix": now_unix(),
        "status": "queued",
        "provider": req.provider,
        "caller": req.caller,
        "stream": req.stream,
        "async": req.async_mode,
        "quiet": req.quiet,
        "message": req.message,
        "timeout_s": req.timeout_s,
        "work_dir": req.work_dir,
    });

    write_json_pretty(&task_file, &content)?;
    Ok(task_file)
}

fn relay_task_dispatched(context: &DaemonContext, req: &AskRequest, req_id: &str) {
    let preview = compact_preview(&req.message, 180);
    let provider_line = format!(
        "[RCCB][任务下发] req_id={} caller={} timeout_s={:.3} msg={}",
        req_id, req.caller, req.timeout_s, preview
    );
    relay_to_provider_feed(
        context,
        &req.provider,
        &provider_line,
    );
    if pane_status_mirror_enabled() {
        let _ = relay_to_provider_pane_status(context, &req.provider, &provider_line);
    }

    if let Some(orchestrator) = current_orchestrator(context) {
        let orchestrator_line = format!(
            "[RCCB][已派发] req_id={} -> provider={} timeout_s={:.3}",
            req_id, req.provider, req.timeout_s
        );
        relay_to_provider_feed(
            context,
            &orchestrator,
            &orchestrator_line,
        );
        if pane_status_mirror_enabled() {
            let _ = relay_to_provider_pane_status(context, &orchestrator, &orchestrator_line);
        }
    }
}

fn relay_task_completed(
    context: &DaemonContext,
    req: &AskRequest,
    req_id: &str,
    status: &str,
    exit_code: i32,
    reply: &str,
) {
    let reply_preview = compact_preview(reply, 200);
    let provider_line = format!(
        "[RCCB][任务完成] req_id={} status={} exit_code={} reply={}",
        req_id, status, exit_code, reply_preview
    );
    relay_to_provider_feed(
        context,
        &req.provider,
        &provider_line,
    );
    if pane_status_mirror_enabled() {
        let _ = relay_to_provider_pane_status(context, &req.provider, &provider_line);
    }

    if let Some(orchestrator) = current_orchestrator(context) {
        let orchestrator_line = format!(
            "[RCCB][执行回传] req_id={} provider={} status={} exit_code={} reply={}",
            req_id, req.provider, status, exit_code, reply_preview
        );
        relay_to_provider_feed(
            context,
            &orchestrator,
            &orchestrator_line,
        );
        if pane_status_mirror_enabled() {
            let _ = relay_to_provider_pane_status(context, &orchestrator, &orchestrator_line);
        }
    }
}

fn relay_to_provider_feed(context: &DaemonContext, provider: &str, line: &str) {
    let feed = launcher_feed_path(&context.project_dir, &context.instance_id, provider);
    if !feed.exists() {
        return;
    }
    let _ = write_line(feed, line);
}

#[derive(Debug, Deserialize)]
struct LauncherMetaView {
    backend: String,
    #[serde(default)]
    backend_bin: Option<String>,
    #[serde(default)]
    providers: Vec<LauncherProviderMetaView>,
}

#[derive(Debug, Deserialize)]
struct LauncherProviderMetaView {
    provider: String,
    #[serde(default)]
    pane_id: Option<String>,
}

#[derive(Debug, Clone)]
enum PaneRelayBackend {
    Tmux,
    Wezterm { bin: String },
}

#[derive(Debug, Clone)]
struct PaneRelayTarget {
    backend: PaneRelayBackend,
    pane_id: String,
}

fn relay_to_provider_pane_status(
    context: &DaemonContext,
    provider: &str,
    line: &str,
) -> Result<()> {
    let Some(target) = resolve_provider_pane_target(context, provider)? else {
        return Ok(());
    };
    let payload = line.trim();
    if payload.is_empty() {
        return Ok(());
    }
    match target.backend {
        PaneRelayBackend::Tmux => {
            let status = ProcessCommand::new("tmux")
                .args(["display-message", "-t", &target.pane_id, payload])
                .status()
                .context("tmux display-message failed")?;
            if status.success() {
                return Ok(());
            }
            bail!(
                "tmux display-message failed: pane={} status={}",
                target.pane_id,
                status
            );
        }
        PaneRelayBackend::Wezterm { bin } => {
            let status = ProcessCommand::new(&bin)
                .args(["cli", "send-text", "--pane-id", &target.pane_id, "--no-paste"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .and_then(|mut child| {
                    if let Some(stdin) = child.stdin.as_mut() {
                        use std::io::Write;
                        let _ = stdin.write_all(payload.as_bytes());
                    }
                    child.wait()
                })
                .with_context(|| format!("wezterm send-text failed: bin={}", bin))?;
            if status.success() {
                return Ok(());
            }
            bail!(
                "wezterm send-text failed: pane={} status={}",
                target.pane_id,
                status
            );
        }
    }
}

fn resolve_provider_pane_target(
    context: &DaemonContext,
    provider: &str,
) -> Result<Option<PaneRelayTarget>> {
    let path = launcher_meta_path(&context.project_dir, &context.instance_id);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("read launcher meta failed: {}", path.display()))?;
    let meta: LauncherMetaView = serde_json::from_str(&raw)
        .with_context(|| format!("parse launcher meta failed: {}", path.display()))?;

    let pane_id = meta
        .providers
        .iter()
        .find(|p| p.provider.trim().eq_ignore_ascii_case(provider.trim()))
        .and_then(|p| p.pane_id.clone())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let Some(pane_id) = pane_id else {
        return Ok(None);
    };

    let backend = match meta.backend.trim().to_ascii_lowercase().as_str() {
        "tmux" => PaneRelayBackend::Tmux,
        "wezterm" => PaneRelayBackend::Wezterm {
            bin: meta
                .backend_bin
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| "wezterm".to_string()),
        },
        _ => return Ok(None),
    };

    Ok(Some(PaneRelayTarget { backend, pane_id }))
}

fn pane_status_mirror_enabled() -> bool {
    env_bool("RCCB_PANE_STATUS_MIRROR", false)
}

fn env_bool(key: &str, default: bool) -> bool {
    let Ok(raw) = std::env::var(key) else {
        return default;
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => default,
    }
}

fn current_orchestrator(context: &DaemonContext) -> Option<String> {
    context
        .shared_state
        .lock()
        .ok()
        .and_then(|s| s.orchestrator.clone())
        .filter(|s| !s.trim().is_empty())
}

fn compact_preview(raw: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let flattened = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();
    if flattened.chars().count() <= max_chars {
        return flattened;
    }
    let mut out: String = flattened.chars().take(max_chars).collect();
    out.push_str("...(截断)");
    out
}

fn debug_log_path(context: &DaemonContext) -> PathBuf {
    logs_instance_dir(&context.project_dir, &context.instance_id).join("debug.log")
}

fn is_debug_enabled(context: &DaemonContext) -> bool {
    context
        .shared_state
        .lock()
        .map(|s| s.debug_enabled)
        .unwrap_or(false)
}

fn debug_log(context: &DaemonContext, line: &str) {
    if !is_debug_enabled(context) {
        return;
    }
    let _ = write_line(debug_log_path(context), line);
}

fn redact_token(mut value: Value) -> Value {
    if let Some(token) = value.get_mut("token") {
        *token = json!("***");
    }
    value
}

fn debug_log_json<T: serde::Serialize>(context: &DaemonContext, prefix: &str, value: &T) {
    if !is_debug_enabled(context) {
        return;
    }
    if let Ok(s) = serde_json::to_string(value) {
        debug_log(context, &format!("{} {}", prefix, s));
    }
}

fn debug_wire_in(context: &DaemonContext, value: &Value) {
    if !is_debug_enabled(context) {
        return;
    }
    debug_log_json(context, "[WIRE][IN]", &redact_token(value.clone()));
}

fn debug_wire_out_value(context: &DaemonContext, value: &Value) {
    debug_log_json(context, "[WIRE][OUT][value]", value);
}

fn debug_wire_out_response(context: &DaemonContext, resp: &AskResponse) {
    debug_log_json(context, "[WIRE][OUT][response]", resp);
}

fn debug_wire_out_event(context: &DaemonContext, evt: &AskEvent) {
    debug_log_json(context, "[WIRE][OUT][event]", evt);
}

fn update_heartbeat(context: &DaemonContext) -> Result<()> {
    let mut guard = context
        .shared_state
        .lock()
        .map_err(|_| anyhow!("state lock poisoned while heartbeat"))?;

    guard.last_heartbeat_unix = now_unix();
    write_state(&context.state_path, &guard)
}

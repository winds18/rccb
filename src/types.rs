use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::constants::{PROTOCOL_PREFIX, PROTOCOL_VERSION};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceState {
    pub schema_version: u32,
    pub instance_id: String,
    pub project_dir: String,
    pub pid: u32,
    pub status: String,
    pub started_at_unix: u64,
    pub last_heartbeat_unix: u64,
    pub stopped_at_unix: Option<u64>,

    #[serde(default)]
    pub providers: Vec<String>,

    #[serde(default)]
    pub orchestrator: Option<String>,

    #[serde(default)]
    pub executors: Vec<String>,

    #[serde(default)]
    pub session_file: Option<String>,

    #[serde(default)]
    pub last_task_id: Option<String>,

    #[serde(default)]
    pub daemon_host: Option<String>,

    #[serde(default)]
    pub daemon_port: Option<u16>,

    #[serde(default)]
    pub daemon_token: Option<String>,

    #[serde(default)]
    pub debug_enabled: bool,
}

#[derive(Debug, Clone)]
pub struct OrchestrationPlan {
    pub providers: Vec<String>,
    pub orchestrator: String,
    pub executors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct OrchestrationArtifacts {
    pub session_file: PathBuf,
    pub task_file: PathBuf,
    pub task_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskRequest {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub v: u32,
    pub id: String,
    pub token: String,
    pub provider: String,
    pub work_dir: String,
    pub timeout_s: f64,
    pub quiet: bool,
    #[serde(default)]
    pub stream: bool,
    pub message: String,
    pub caller: String,
    pub req_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskResponse {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub v: u32,
    pub id: String,
    pub req_id: Option<String>,
    pub exit_code: i32,
    pub reply: String,
    pub provider: Option<String>,
    pub meta: Option<Value>,
}

impl AskResponse {
    pub fn unauthorized(id: String) -> Self {
        Self {
            msg_type: format!("{}.response", PROTOCOL_PREFIX),
            v: PROTOCOL_VERSION,
            id,
            req_id: None,
            exit_code: 1,
            reply: "Unauthorized".to_string(),
            provider: None,
            meta: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskEvent {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub v: u32,
    pub id: String,
    pub req_id: Option<String>,
    pub provider: Option<String>,
    pub event: String,
    pub delta: Option<String>,
    pub reply: Option<String>,
    pub exit_code: Option<i32>,
    pub meta: Option<Value>,
}

impl AskEvent {
    pub fn start(id: String, req_id: String, provider: String, meta: Option<Value>) -> Self {
        Self {
            msg_type: format!("{}.event", PROTOCOL_PREFIX),
            v: PROTOCOL_VERSION,
            id,
            req_id: Some(req_id),
            provider: Some(provider),
            event: "start".to_string(),
            delta: None,
            reply: None,
            exit_code: None,
            meta,
        }
    }

    pub fn delta(id: String, req_id: String, provider: String, delta: String) -> Self {
        Self {
            msg_type: format!("{}.event", PROTOCOL_PREFIX),
            v: PROTOCOL_VERSION,
            id,
            req_id: Some(req_id),
            provider: Some(provider),
            event: "delta".to_string(),
            delta: Some(delta),
            reply: None,
            exit_code: None,
            meta: None,
        }
    }

    pub fn done(id: String, resp: AskResponse) -> Self {
        Self {
            msg_type: format!("{}.event", PROTOCOL_PREFIX),
            v: PROTOCOL_VERSION,
            id,
            req_id: resp.req_id,
            provider: resp.provider,
            event: "done".to_string(),
            delta: None,
            reply: Some(resp.reply),
            exit_code: Some(resp.exit_code),
            meta: resp.meta,
        }
    }

    pub fn error(
        id: String,
        req_id: Option<String>,
        provider: Option<String>,
        reply: String,
    ) -> Self {
        Self {
            msg_type: format!("{}.event", PROTOCOL_PREFIX),
            v: PROTOCOL_VERSION,
            id,
            req_id,
            provider,
            event: "error".to_string(),
            delta: None,
            reply: Some(reply),
            exit_code: Some(1),
            meta: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum WorkerEvent {
    Start {
        provider: String,
        req_id: String,
        meta: Option<Value>,
    },
    Delta {
        provider: String,
        req_id: String,
        delta: String,
    },
    Done {
        response: AskResponse,
    },
    Error {
        provider: String,
        req_id: String,
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct WorkerTask {
    pub request: AskRequest,
    pub req_id: String,
    pub task_file: PathBuf,
    pub response_tx: Option<mpsc::Sender<AskResponse>>,
    pub stream_tx: Option<mpsc::Sender<WorkerEvent>>,
}

#[derive(Clone)]
pub struct DaemonContext {
    pub project_dir: PathBuf,
    pub instance_id: String,
    pub state_path: PathBuf,
    pub shared_state: Arc<Mutex<InstanceState>>,
    pub allowed_providers: Vec<String>,
}

#[derive(Debug)]
pub struct PendingStream {
    pub event_rx: mpsc::Receiver<WorkerEvent>,
    pub timeout_s: f64,
}

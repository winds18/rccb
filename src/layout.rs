use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

pub fn rccb_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(".rccb")
}

pub fn run_dir(project_dir: &Path) -> PathBuf {
    rccb_dir(project_dir).join("run")
}

pub fn logs_root_dir(project_dir: &Path) -> PathBuf {
    rccb_dir(project_dir).join("logs")
}

pub fn sessions_root_dir(project_dir: &Path) -> PathBuf {
    rccb_dir(project_dir).join("sessions")
}

pub fn tasks_root_dir(project_dir: &Path) -> PathBuf {
    rccb_dir(project_dir).join("tasks")
}

pub fn tmp_root_dir(project_dir: &Path) -> PathBuf {
    rccb_dir(project_dir).join("tmp")
}

pub fn update_root_dir(project_dir: &Path) -> PathBuf {
    rccb_dir(project_dir).join("update")
}

pub fn update_cache_path(project_dir: &Path) -> PathBuf {
    update_root_dir(project_dir).join("last_check.json")
}

pub fn update_tmp_dir(project_dir: &Path) -> PathBuf {
    tmp_root_dir(project_dir).join("update")
}

pub fn session_instance_dir(project_dir: &Path, instance: &str) -> PathBuf {
    sessions_root_dir(project_dir).join(sanitize_instance(instance))
}

pub fn tasks_instance_dir(project_dir: &Path, instance: &str) -> PathBuf {
    tasks_root_dir(project_dir).join(sanitize_instance(instance))
}

pub fn task_artifacts_dir(project_dir: &Path, instance: &str) -> PathBuf {
    tasks_instance_dir(project_dir, instance).join("artifacts")
}

pub fn task_request_artifact_path(project_dir: &Path, instance: &str, req_id: &str) -> PathBuf {
    task_artifacts_dir(project_dir, instance)
        .join(format!("{}.request.md", sanitize_filename(req_id)))
}

pub fn task_reply_artifact_path(project_dir: &Path, instance: &str, req_id: &str) -> PathBuf {
    task_artifacts_dir(project_dir, instance)
        .join(format!("{}.reply.md", sanitize_filename(req_id)))
}

pub fn tmp_instance_dir(project_dir: &Path, instance: &str) -> PathBuf {
    tmp_root_dir(project_dir).join(sanitize_instance(instance))
}

pub fn provider_request_dir(project_dir: &Path, provider: &str) -> PathBuf {
    tmp_root_dir(project_dir)
        .join(sanitize_filename(
            provider.trim().to_ascii_lowercase().as_str(),
        ))
        .join("requests")
}

pub fn provider_request_file(project_dir: &Path, provider: &str, req_id: &str) -> PathBuf {
    provider_request_dir(project_dir, provider).join(format!("{}.md", sanitize_filename(req_id)))
}

pub fn launcher_dir(project_dir: &Path, instance: &str) -> PathBuf {
    tmp_instance_dir(project_dir, instance).join("launcher")
}

pub fn launcher_meta_path(project_dir: &Path, instance: &str) -> PathBuf {
    launcher_dir(project_dir, instance).join("meta.json")
}

pub fn launcher_feed_dir(project_dir: &Path, instance: &str) -> PathBuf {
    launcher_dir(project_dir, instance).join("feeds")
}

pub fn launcher_feed_path(project_dir: &Path, instance: &str, provider: &str) -> PathBuf {
    launcher_feed_dir(project_dir, instance).join(format!(
        "{}.log",
        sanitize_filename(provider.trim().to_ascii_lowercase().as_str())
    ))
}

pub fn logs_instance_dir(project_dir: &Path, instance: &str) -> PathBuf {
    logs_root_dir(project_dir).join(sanitize_instance(instance))
}

pub fn state_path(project_dir: &Path, instance: &str) -> PathBuf {
    run_dir(project_dir).join(format!("{}.json", sanitize_instance(instance)))
}

pub fn lock_path(project_dir: &Path, instance: &str) -> PathBuf {
    run_dir(project_dir).join(format!("{}.lock", sanitize_instance(instance)))
}

pub fn ensure_project_layout(project_dir: &Path) -> Result<()> {
    fs::create_dir_all(rccb_dir(project_dir))?;
    fs::create_dir_all(run_dir(project_dir))?;
    fs::create_dir_all(logs_root_dir(project_dir))?;
    fs::create_dir_all(sessions_root_dir(project_dir))?;
    fs::create_dir_all(tasks_root_dir(project_dir))?;
    fs::create_dir_all(tmp_root_dir(project_dir))?;
    fs::create_dir_all(update_root_dir(project_dir))?;
    Ok(())
}

pub fn sanitize_instance(instance: &str) -> String {
    let x = instance.trim();
    if x.is_empty() {
        return "default".to_string();
    }

    x.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

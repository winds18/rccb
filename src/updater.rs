use std::cmp::Ordering;
use std::env;
use std::fs::{self, File};
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, USER_AGENT};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::Archive;

use crate::cli::Command;
use crate::io_utils::{now_unix, write_json_pretty};
use crate::layout::{ensure_project_layout, sanitize_filename, update_cache_path, update_tmp_dir};

const DEFAULT_UPDATE_REPO: &str = "winds18/rccb";
const DEFAULT_CHECK_INTERVAL_HOURS: u64 = 24;
const AUTO_CHECK_TIMEOUT_SECS: u64 = 2;
const EXPLICIT_CHECK_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Clone, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdateCache {
    checked_at_unix: u64,
    include_prerelease: bool,
    current_version: String,
    latest_tag: String,
    latest_version: String,
    #[serde(default)]
    release_url: Option<String>,
    #[serde(default)]
    asset_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct UpdateCheckView {
    current_version: String,
    latest_tag: String,
    latest_version: String,
    newer_available: bool,
    prerelease: bool,
    release_url: Option<String>,
    asset_name: String,
}

#[derive(Debug, Clone)]
struct ReleaseSelection {
    tag: String,
    version: String,
    prerelease: bool,
    release_url: Option<String>,
    asset_name: String,
    asset_url: String,
    checksum_url: String,
}

pub fn maybe_auto_update_notice(project_dir: &Path, command: Option<&Command>) {
    if !auto_update_allowed_for_command(command) || !env_bool("RCCB_AUTO_UPDATE_CHECK", true) {
        return;
    }
    if ensure_project_layout(project_dir).is_err() {
        return;
    }

    let include_prerelease = env_bool("RCCB_UPDATE_INCLUDE_PRERELEASE", true);
    let current = current_version().to_string();
    let cache_path = update_cache_path(project_dir);
    let interval = Duration::from_secs(update_check_interval_hours() * 3600);

    if let Ok(Some(cache)) = load_update_cache(&cache_path) {
        if cache_has_pending_update(&cache, &current, include_prerelease) {
            print_update_notice(&cache.latest_tag, &current, true);
            return;
        }
        let age = now_unix().saturating_sub(cache.checked_at_unix);
        if cache.current_version == current
            && cache.include_prerelease == include_prerelease
            && age < interval.as_secs()
        {
            return;
        }
    }

    let client = match build_update_http_client(AUTO_CHECK_TIMEOUT_SECS) {
        Ok(v) => v,
        Err(_) => return,
    };
    let release = match fetch_latest_release(&client, include_prerelease) {
        Ok(v) => v,
        Err(_) => return,
    };

    let cache = UpdateCache {
        checked_at_unix: now_unix(),
        include_prerelease,
        current_version: current.clone(),
        latest_tag: release.tag.clone(),
        latest_version: release.version.clone(),
        release_url: release.release_url.clone(),
        asset_name: Some(release.asset_name.clone()),
    };
    let _ = write_json_pretty(&cache_path, &cache);

    if version_cmp(&release.version, &current) == Ordering::Greater {
        print_update_notice(&release.tag, &current, false);
    }
}

pub fn cmd_update_check(project_dir: &Path, as_json: bool) -> Result<()> {
    ensure_project_layout(project_dir)?;
    let include_prerelease = env_bool("RCCB_UPDATE_INCLUDE_PRERELEASE", true);
    let client = build_update_http_client(EXPLICIT_CHECK_TIMEOUT_SECS)?;
    let release = fetch_latest_release(&client, include_prerelease)?;
    let current = current_version().to_string();
    let newer = version_cmp(&release.version, &current) == Ordering::Greater;

    let cache = UpdateCache {
        checked_at_unix: now_unix(),
        include_prerelease,
        current_version: current.clone(),
        latest_tag: release.tag.clone(),
        latest_version: release.version.clone(),
        release_url: release.release_url.clone(),
        asset_name: Some(release.asset_name.clone()),
    };
    write_json_pretty(&update_cache_path(project_dir), &cache)?;

    let view = UpdateCheckView {
        current_version: current.clone(),
        latest_tag: release.tag.clone(),
        latest_version: release.version.clone(),
        newer_available: newer,
        prerelease: release.prerelease,
        release_url: release.release_url.clone(),
        asset_name: release.asset_name.clone(),
    };

    if as_json {
        println!("{}", serde_json::to_string_pretty(&view)?);
        return Ok(());
    }

    if newer {
        println!(
            "发现新版本：{}（当前 {}）\n平台产物：{}\n更新命令：rccb --project-dir {} update apply",
            release.tag,
            current,
            release.asset_name,
            project_dir.display()
        );
        if let Some(url) = release.release_url {
            println!("发布页：{}", url);
        }
    } else {
        println!(
            "当前已是最新版本：{}（平台产物：{}）",
            current, release.asset_name
        );
    }
    Ok(())
}

pub fn cmd_update_apply(
    project_dir: &Path,
    version: Option<&str>,
    install_path: Option<&Path>,
    force: bool,
) -> Result<()> {
    ensure_project_layout(project_dir)?;
    let include_prerelease = env_bool("RCCB_UPDATE_INCLUDE_PRERELEASE", true);
    let client = build_update_http_client(EXPLICIT_CHECK_TIMEOUT_SECS)?;
    let release = match version {
        Some(tag) => fetch_release_by_tag(&client, tag)?,
        None => fetch_latest_release(&client, include_prerelease)?,
    };

    let current = current_version().to_string();
    let target_path = resolve_install_path(install_path)?;
    if !force && version_cmp(&release.version, &current) != Ordering::Greater {
        println!(
            "当前版本 {} 已不低于目标版本 {}，未执行安装。若需覆盖安装，请追加 --force。",
            current, release.tag
        );
        return Ok(());
    }

    let work_dir = update_tmp_dir(project_dir).join(sanitize_filename(&release.tag));
    if work_dir.exists() {
        let _ = fs::remove_dir_all(&work_dir);
    }
    fs::create_dir_all(&work_dir)?;

    let asset_archive = work_dir.join(&release.asset_name);
    let checksum_file = work_dir.join("SHA256SUMS.txt");
    let unpacked_binary = work_dir.join(platform_binary_name()?);

    download_to_path(&client, &release.asset_url, &asset_archive)?;
    download_to_path(&client, &release.checksum_url, &checksum_file)?;
    verify_download_checksum(&asset_archive, &checksum_file, &release.asset_name)?;
    unpack_release_archive(&asset_archive, &unpacked_binary)?;
    install_binary(&unpacked_binary, &target_path)?;

    println!(
        "更新完成：{} -> {}\n安装路径：{}",
        current,
        release.tag,
        target_path.display()
    );
    if let Some(url) = release.release_url {
        println!("发布页：{}", url);
    }
    Ok(())
}

fn auto_update_allowed_for_command(command: Option<&Command>) -> bool {
    match command {
        None => true,
        Some(
            Command::Init { .. }
            | Command::Start { .. }
            | Command::Status { .. }
            | Command::Mounted { .. }
            | Command::Tasks { .. }
            | Command::Inbox { .. }
            | Command::Watch { .. }
            | Command::Stop { .. }
            | Command::Ping { .. }
            | Command::Debug { .. },
        ) => true,
        Some(_) => false,
    }
}

fn build_update_http_client(timeout_secs: u64) -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(&format!("rccb/{}", current_version()))
            .context("构建 User-Agent 失败")?,
    );
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(timeout_secs.max(1)))
        .build()
        .context("构建更新 HTTP 客户端失败")
}

fn fetch_latest_release(client: &Client, include_prerelease: bool) -> Result<ReleaseSelection> {
    let repo = update_repo();
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=20");
    let releases: Vec<GitHubRelease> = client
        .get(url)
        .send()
        .context("获取 release 列表失败")?
        .error_for_status()
        .context("release 列表请求失败")?
        .json()
        .context("解析 release 列表失败")?;

    let mut best: Option<ReleaseSelection> = None;
    for release in releases {
        if release.draft || (!include_prerelease && release.prerelease) {
            continue;
        }
        let candidate = match select_release_for_current_platform(release) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if best
            .as_ref()
            .map(|cur| version_cmp(&candidate.version, &cur.version) == Ordering::Greater)
            .unwrap_or(true)
        {
            best = Some(candidate);
        }
    }
    best.ok_or_else(|| anyhow!("未找到适用于当前平台的可更新版本"))
}

fn fetch_release_by_tag(client: &Client, raw_tag: &str) -> Result<ReleaseSelection> {
    let repo = update_repo();
    let tag = normalize_tag(raw_tag);
    let url = format!("https://api.github.com/repos/{repo}/releases/tags/{tag}");
    let release: GitHubRelease = client
        .get(url)
        .send()
        .with_context(|| format!("获取指定版本失败：{}", tag))?
        .error_for_status()
        .with_context(|| format!("指定版本不存在或不可访问：{}", tag))?
        .json()
        .context("解析指定版本信息失败")?;
    select_release_for_current_platform(release)
}

fn select_release_for_current_platform(release: GitHubRelease) -> Result<ReleaseSelection> {
    let version = normalize_version(&release.tag_name);
    let asset_name = platform_asset_name_for_version(&version)?;
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .cloned()
        .ok_or_else(|| anyhow!("release {} 缺少平台产物 {}", release.tag_name, asset_name))?;
    let checksum = release
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS.txt")
        .cloned()
        .ok_or_else(|| anyhow!("release {} 缺少 SHA256SUMS.txt", release.tag_name))?;
    Ok(ReleaseSelection {
        tag: release.tag_name.clone(),
        version,
        prerelease: release.prerelease,
        release_url: release.html_url.clone(),
        asset_name,
        asset_url: asset.browser_download_url,
        checksum_url: checksum.browser_download_url,
    })
}

fn update_repo() -> String {
    env::var("RCCB_UPDATE_REPO").unwrap_or_else(|_| DEFAULT_UPDATE_REPO.to_string())
}

fn normalize_tag(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with('v') {
        trimmed.to_string()
    } else {
        format!("v{}", trimmed)
    }
}

fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn normalize_version(raw: &str) -> String {
    raw.trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()
        .unwrap_or(raw.trim())
        .to_string()
}

fn parse_version_triplet(raw: &str) -> Option<(u64, u64, u64)> {
    let norm = normalize_version(raw);
    let mut parts = norm.split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

fn version_cmp(a: &str, b: &str) -> Ordering {
    match (parse_version_triplet(a), parse_version_triplet(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        _ => normalize_version(a).cmp(&normalize_version(b)),
    }
}

fn platform_asset_name_for_version(version: &str) -> Result<String> {
    match (env::consts::OS, env::consts::ARCH) {
        ("macos", "aarch64") => Ok(format!("rccb-v{}-macos-arm64.tar.gz", version)),
        ("linux", "x86_64") => Ok(format!("rccb-v{}-linux-x86_64.tar.gz", version)),
        (os, arch) => bail!("当前平台暂不支持自动更新：{} {}", os, arch),
    }
}

fn platform_binary_name() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("macos", "aarch64") => Ok("rccb-macos-arm64"),
        ("linux", "x86_64") => Ok("rccb-linux-x86_64"),
        (os, arch) => bail!("当前平台暂不支持自动更新：{} {}", os, arch),
    }
}

fn resolve_install_path(install_path: Option<&Path>) -> Result<PathBuf> {
    let path = match install_path {
        Some(path) => {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                env::current_dir()?.join(path)
            }
        }
        None => {
            let exe = env::current_exe().context("获取当前可执行文件路径失败")?;
            if looks_like_dev_binary_path(&exe) {
                bail!(
                    "当前正在运行开发构建：{}\n为避免误覆盖调试二进制，请显式传入 --install-path。",
                    exe.display()
                );
            }
            exe
        }
    };
    Ok(path)
}

fn looks_like_dev_binary_path(path: &Path) -> bool {
    let text = path.display().to_string();
    text.contains("/target/debug/") || text.contains("/target/release/")
}

fn download_to_path(client: &Client, url: &str, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut resp = client
        .get(url)
        .send()
        .with_context(|| format!("下载失败：{}", url))?
        .error_for_status()
        .with_context(|| format!("下载响应失败：{}", url))?;
    let mut file =
        File::create(path).with_context(|| format!("创建文件失败：{}", path.display()))?;
    io::copy(&mut resp, &mut file).with_context(|| format!("写入文件失败：{}", path.display()))?;
    file.flush()
        .with_context(|| format!("刷新文件失败：{}", path.display()))?;
    Ok(())
}

fn verify_download_checksum(
    archive_path: &Path,
    checksum_path: &Path,
    asset_name: &str,
) -> Result<()> {
    let expected = load_expected_checksum(checksum_path, asset_name)?;
    let actual = sha256_file_hex(archive_path)?;
    if actual != expected.to_ascii_lowercase() {
        bail!(
            "校验失败：{}\nexpected={}\nactual={}",
            asset_name,
            expected,
            actual
        );
    }
    Ok(())
}

fn load_expected_checksum(checksum_path: &Path, asset_name: &str) -> Result<String> {
    let raw = fs::read_to_string(checksum_path)
        .with_context(|| format!("读取校验文件失败：{}", checksum_path.display()))?;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let hash = match parts.next() {
            Some(v) => v,
            None => continue,
        };
        let name = match parts.next_back().or_else(|| parts.next()) {
            Some(v) => v.trim_start_matches('*'),
            None => continue,
        };
        if name == asset_name {
            return Ok(hash.to_ascii_lowercase());
        }
    }
    bail!("SHA256SUMS.txt 中未找到目标产物：{}", asset_name)
}

fn sha256_file_hex(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("打开文件失败：{}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("读取文件失败：{}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn unpack_release_archive(archive_path: &Path, output_path: &Path) -> Result<()> {
    let bytes = fs::read(archive_path)
        .with_context(|| format!("读取压缩包失败：{}", archive_path.display()))?;
    let cursor = Cursor::new(bytes);
    let decoder = GzDecoder::new(cursor);
    let mut archive = Archive::new(decoder);
    let expected_name = platform_binary_name()?;
    let mut matched = false;

    for entry in archive.entries().context("读取压缩包条目失败")? {
        let mut entry = entry.context("读取压缩包条目失败")?;
        let path = entry.path().context("读取压缩包路径失败")?;
        let file_name = path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or_default();
        if file_name != expected_name {
            continue;
        }
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::create(output_path)
            .with_context(|| format!("创建文件失败：{}", output_path.display()))?;
        io::copy(&mut entry, &mut out)
            .with_context(|| format!("解包失败：{}", output_path.display()))?;
        out.flush()
            .with_context(|| format!("刷新文件失败：{}", output_path.display()))?;
        matched = true;
        break;
    }

    if !matched {
        bail!("压缩包中未找到可执行文件：{}", expected_name);
    }
    set_executable(output_path)?;
    Ok(())
}

fn install_binary(source_path: &Path, install_path: &Path) -> Result<()> {
    let parent = install_path
        .parent()
        .ok_or_else(|| anyhow!("无效安装路径：{}", install_path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("创建安装目录失败：{}", parent.display()))?;

    let tmp_path = install_path.with_extension("rccb-update-tmp");
    fs::copy(source_path, &tmp_path).with_context(|| {
        format!(
            "复制新二进制失败：{} -> {}",
            source_path.display(),
            tmp_path.display()
        )
    })?;
    set_executable(&tmp_path)?;
    fs::rename(&tmp_path, install_path).with_context(|| {
        format!(
            "安装失败：{} -> {}",
            tmp_path.display(),
            install_path.display()
        )
    })?;
    Ok(())
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

fn load_update_cache(path: &Path) -> Result<Option<UpdateCache>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("读取更新缓存失败：{}", path.display()))?;
    let cache = serde_json::from_str(&raw)
        .with_context(|| format!("解析更新缓存失败：{}", path.display()))?;
    Ok(Some(cache))
}

fn update_check_interval_hours() -> u64 {
    env::var("RCCB_UPDATE_CHECK_INTERVAL_HOURS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_CHECK_INTERVAL_HOURS)
        .clamp(1, 24 * 30)
}

fn cache_has_pending_update(
    cache: &UpdateCache,
    current_version: &str,
    include_prerelease: bool,
) -> bool {
    cache.include_prerelease == include_prerelease
        && version_cmp(&cache.latest_version, current_version) == Ordering::Greater
}

fn print_update_notice(latest_tag: &str, current_version: &str, from_cache: bool) {
    if from_cache {
        eprintln!(
            "提示：发现新版本 {}（当前 {}）。已记录到本地 `.rccb`，本次直接使用本地提醒。可执行 `rccb update apply` 自动更新。",
            latest_tag, current_version
        );
    } else {
        eprintln!(
            "提示：发现新版本 {}（当前 {}）。已记录到本地 `.rccb`，后续启动会直接本地提醒。可执行 `rccb update apply` 自动更新。",
            latest_tag, current_version
        );
    }
}

fn env_bool(name: &str, default: bool) -> bool {
    match env::var(name) {
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => default,
        },
        Err(_) => default,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        cache_has_pending_update, load_expected_checksum, normalize_tag, normalize_version,
        parse_version_triplet, version_cmp, UpdateCache,
    };
    use std::cmp::Ordering;
    use std::fs;

    #[test]
    fn normalize_version_strips_v_and_suffix() {
        assert_eq!(normalize_version("v0.1.2"), "0.1.2");
        assert_eq!(normalize_version("0.1.2-beta.1"), "0.1.2");
    }

    #[test]
    fn normalize_tag_adds_v_prefix() {
        assert_eq!(normalize_tag("0.1.1"), "v0.1.1");
        assert_eq!(normalize_tag("v0.1.1"), "v0.1.1");
    }

    #[test]
    fn parse_version_triplet_reads_semver() {
        assert_eq!(parse_version_triplet("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version_triplet("1.2"), None);
    }

    #[test]
    fn version_cmp_uses_numeric_order() {
        assert_eq!(version_cmp("0.1.10", "0.1.9"), Ordering::Greater);
        assert_eq!(version_cmp("v0.1.1", "0.1.1"), Ordering::Equal);
    }

    #[test]
    fn load_expected_checksum_finds_target_asset() {
        let path = std::env::temp_dir().join("rccb-sha256sums-test.txt");
        fs::write(
            &path,
            "aaaa1111  rccb-v0.1.1-macos-arm64.tar.gz\nbbbb2222  rccb-v0.1.1-linux-x86_64.tar.gz\n",
        )
        .unwrap();
        let got =
            load_expected_checksum(&path, "rccb-v0.1.1-linux-x86_64.tar.gz").expect("checksum");
        assert_eq!(got, "bbbb2222");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn cache_has_pending_update_requires_newer_cached_version() {
        let cache = UpdateCache {
            checked_at_unix: 1,
            include_prerelease: true,
            current_version: "0.1.1".to_string(),
            latest_tag: "v0.1.2".to_string(),
            latest_version: "0.1.2".to_string(),
            release_url: None,
            asset_name: Some("asset".to_string()),
        };
        assert!(cache_has_pending_update(&cache, "0.1.1", true));
        assert!(!cache_has_pending_update(&cache, "0.1.2", true));
        assert!(!cache_has_pending_update(&cache, "0.1.1", false));
    }
}

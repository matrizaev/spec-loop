use std::env;
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Command;

use uuid::Uuid;

use crate::error::{Result, SpecLoopError, io_error};

const AUTH_DIRECTORY_ENV: &str = "SPEC_LOOP_AUTH_DIR";

pub(super) fn normalize_host_id(host_id: &str) -> Option<String> {
    let uuid_text = host_id.strip_prefix("urn:uuid:").unwrap_or(host_id);
    let uuid = Uuid::parse_str(uuid_text).ok()?;
    if uuid.get_version_num() != 4 || uuid.hyphenated().to_string() != uuid_text {
        return None;
    }
    Some(format!("urn:uuid:{uuid}"))
}

pub(super) fn restrict_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|source| io_error("restrict authentication directory", path, source))?;
    }
    #[cfg(windows)]
    restrict_windows_acl(path, true)?;
    Ok(())
}

pub(super) fn restrict_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|source| io_error("restrict authentication file", path, source))?;
    }
    #[cfg(windows)]
    restrict_windows_acl(path, false)?;
    Ok(())
}

#[cfg(windows)]
fn restrict_windows_acl(path: &Path, directory: bool) -> Result<()> {
    let sid = current_windows_user_sid()?;
    let reset = Command::new("icacls.exe")
        .arg(path)
        .arg("/reset")
        .output()
        .map_err(|source| io_error("reset credential ACL", path, source))?;
    if !reset.status.success() {
        return Err(SpecLoopError::Workflow(format!(
            "could not reset credential ACL for {}",
            path.display()
        )));
    }
    let user_permissions = if directory { "(OI)(CI)(F)" } else { "(F)" };
    let user_grant = format!("*{sid}:{user_permissions}");
    let restricted = Command::new("icacls.exe")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(user_grant)
        .arg("*S-1-5-18:(F)")
        .output()
        .map_err(|source| io_error("restrict credential ACL", path, source))?;
    if !restricted.status.success() {
        return Err(SpecLoopError::Workflow(format!(
            "could not restrict credential ACL for {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn current_windows_user_sid() -> Result<String> {
    let identity = Command::new("whoami.exe")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .map_err(|source| io_error("identify current Windows user", "whoami.exe", source))?;
    if !identity.status.success() {
        return Err(SpecLoopError::Workflow(
            "could not identify current Windows user for credential permissions".to_owned(),
        ));
    }
    let identity = std::str::from_utf8(&identity.stdout).map_err(|error| {
        SpecLoopError::Workflow(format!("Windows user identity output is invalid: {error}"))
    })?;
    identity
        .trim()
        .trim_start_matches('\u{feff}')
        .split_once(',')
        .map(|(_, sid)| sid.trim().trim_matches('"'))
        .filter(|sid| valid_windows_sid(sid))
        .map(str::to_owned)
        .ok_or_else(|| {
            SpecLoopError::Workflow(
                "could not parse current Windows user SID for credential permissions".to_owned(),
            )
        })
}

#[cfg(windows)]
fn valid_windows_sid(sid: &str) -> bool {
    let mut parts = sid.split('-');
    parts.next() == Some("S")
        && parts.all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

pub(super) fn registry_path() -> Result<PathBuf> {
    let current_dir = env::current_dir().map_err(|source| {
        io_error(
            "resolve current directory for authentication storage",
            ".",
            source,
        )
    })?;
    let project_root = project_boundary(&current_dir)?;
    if let Some(auth_dir) = env::var_os(AUTH_DIRECTORY_ENV) {
        return registry_path_for_auth_dir(Path::new(&auth_dir), project_root.as_deref());
    }
    #[cfg(target_os = "macos")]
    let path = {
        let home = env::var_os("HOME").ok_or_else(|| {
            SpecLoopError::Config(
                "HOME is unavailable; cannot locate authentication storage".to_owned(),
            )
        })?;
        PathBuf::from(home).join("Library/Application Support/Spec Loop/auth.json")
    };
    #[cfg(target_os = "windows")]
    let path = {
        let app_data = env::var_os("APPDATA").ok_or_else(|| {
            SpecLoopError::Config(
                "APPDATA is unavailable; cannot locate authentication storage".to_owned(),
            )
        })?;
        PathBuf::from(app_data).join("Spec Loop/auth.json")
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let config_dir = if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
            PathBuf::from(xdg)
        } else {
            let home = env::var_os("HOME").ok_or_else(|| {
                SpecLoopError::Config(
                    "HOME is unavailable; cannot locate authentication storage".to_owned(),
                )
            })?;
            PathBuf::from(home).join(".config")
        };
        registry_path_for_config_dir(&config_dir, project_root.as_deref())
    }
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        validate_external_registry_path(&path, project_root.as_deref())
    }
}

pub(super) fn project_boundary(current_dir: &Path) -> Result<Option<PathBuf>> {
    for directory in current_dir.ancestors() {
        let profile = directory.join(".spec-loop.toml");
        match fs::metadata(&profile) {
            Ok(_) => return Ok(Some(directory.to_path_buf())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error("inspect project profile", &profile, source)),
        }
    }
    Ok(None)
}

pub(super) fn registry_path_for_auth_dir(
    auth_dir: &Path,
    project_root: Option<&Path>,
) -> Result<PathBuf> {
    if auth_dir.as_os_str().is_empty() || !auth_dir.is_absolute() {
        return Err(SpecLoopError::Config(format!(
            "{AUTH_DIRECTORY_ENV} must be a non-empty absolute directory outside the active project; it directly contains auth.json"
        )));
    }
    validate_external_registry_path(&auth_dir.join("auth.json"), project_root)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(super) fn registry_path_for_config_dir(
    config_dir: &Path,
    project_root: Option<&Path>,
) -> Result<PathBuf> {
    validate_external_registry_path(&config_dir.join("spec-loop/auth.json"), project_root)
}

pub(super) fn validate_external_registry_path(
    path: &Path,
    project_root: Option<&Path>,
) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(SpecLoopError::Config(format!(
            "authentication storage path must be absolute and outside the project: {}",
            path.display()
        )));
    }
    let normalized = normalize_absolute_path(path);
    let Some(project_root) = project_root else {
        return Ok(normalized);
    };
    let project_root = fs::canonicalize(project_root).map_err(|source| {
        io_error(
            "resolve project root for authentication storage",
            project_root,
            source,
        )
    })?;
    let mut unresolved = normalized.as_path();
    let mut missing_components = Vec::new();
    let resolved = loop {
        match fs::canonicalize(unresolved) {
            Ok(mut existing) => {
                for component in missing_components.iter().rev() {
                    existing.push(component);
                }
                break existing;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = unresolved.file_name().ok_or_else(|| {
                    SpecLoopError::Config(format!(
                        "cannot resolve authentication storage path: {}",
                        path.display()
                    ))
                })?;
                missing_components.push(name.to_os_string());
                unresolved = unresolved.parent().ok_or_else(|| {
                    SpecLoopError::Config(format!(
                        "cannot resolve authentication storage path: {}",
                        path.display()
                    ))
                })?;
            }
            Err(source) => {
                return Err(io_error(
                    "resolve authentication storage path",
                    unresolved,
                    source,
                ));
            }
        }
    };
    if resolved.starts_with(&project_root) {
        return Err(SpecLoopError::Config(format!(
            "authentication storage must remain outside the project directory: {}",
            path.display()
        )));
    }
    Ok(normalized)
}

fn normalize_absolute_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                normalized.push(component.as_os_str());
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::Normal(value) => normalized.push(value),
        }
    }
    normalized
}

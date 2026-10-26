use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::future::Future;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use openidconnect::core::{
    CoreAuthenticationFlow, CoreClient, CoreIdTokenVerifier, CoreProviderMetadata,
    CoreTokenResponse,
};
use openidconnect::{
    AuthorizationCode, ClientId, CsrfToken, EndpointMaybeSet, EndpointNotSet, EndpointSet,
    IssuerUrl, Nonce, OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl,
    RefreshToken, Scope, TokenResponse,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use crate::error::{Result, SpecLoopError, io_error};
use crate::runtime;

const ISSUER: &str = "https://auth.openai.com";
const CALLBACK_PATH: &str = "/auth/callback";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_CALLBACK_BYTES: usize = 16 * 1024;
const DIRECT_SCOPE: &str = "chatgpt.tokens.use.direct";
const KEYRING_SERVICE: &str = "spec-loop-chatgpt";
type ConfiguredCoreClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// Auth command requested by the profile-independent CLI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthCommand {
    /// Complete browser-based `ChatGPT` authorization.
    Login {
        /// Reauthorize a saved registration ID.
        account: Option<String>,
        /// Start a distinct dynamic registration.
        new_account: bool,
    },
    /// Report local credential status without revealing credentials.
    Status,
    /// Remove selected or active Spec Loop credentials.
    Logout {
        /// Opaque registration ID to remove locally.
        account: Option<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    version: u32,
    host_id: String,
    registrations: Vec<Registration>,
    active_registration_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    id: String,
    issuer: String,
    subject: String,
    client_id: String,
    credential_storage: CredentialStorage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fallback_credential: Option<Credential>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CredentialStorage {
    Keyring,
    File,
    LoggedOut,
}

enum CredentialWriteError {
    Unavailable,
    Failed(SpecLoopError),
}

impl CredentialWriteError {
    fn into_error(self) -> SpecLoopError {
        match self {
            Self::Unavailable => SpecLoopError::Workflow(
                "OS credential store is unavailable; run `spec-loop auth login` after restoring store access"
                    .to_owned(),
            ),
            Self::Failed(error) => error,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Credential {
    access_token: String,
    refresh_token: Option<String>,
    id_token: String,
    scopes: Vec<String>,
    expires_at_unix: Option<u64>,
}

impl Registry {
    fn validate(&self, path: &Path) -> Result<()> {
        if normalize_host_id(&self.host_id).as_deref() != Some(self.host_id.as_str()) {
            return Err(SpecLoopError::Config(format!(
                "{}: authentication host_id is malformed",
                path.display()
            )));
        }
        let mut registration_ids = BTreeSet::new();
        for registration in &self.registrations {
            if registration.issuer != ISSUER
                || registration.subject.is_empty()
                || registration.client_id.trim().is_empty()
            {
                return Err(SpecLoopError::Config(format!(
                    "{}: ChatGPT registration metadata contains an unsupported issuer or empty identity field",
                    path.display()
                )));
            }
            let expected = registration_id(
                &registration.issuer,
                &registration.subject,
                &registration.client_id,
            )?;
            if registration.id != expected || !registration_ids.insert(&registration.id) {
                return Err(SpecLoopError::Config(format!(
                    "{}: invalid, duplicate, or identity-mismatched ChatGPT registration metadata",
                    path.display()
                )));
            }
            match registration.credential_storage {
                CredentialStorage::File if registration.fallback_credential.is_some() => {
                    let credential =
                        registration.fallback_credential.as_ref().ok_or_else(|| {
                            SpecLoopError::Config(
                                "missing file-backed ChatGPT credential".to_owned(),
                            )
                        })?;
                    validate_credential(credential)?;
                }
                CredentialStorage::Keyring | CredentialStorage::LoggedOut
                    if registration.fallback_credential.is_none() => {}
                _ => {
                    return Err(SpecLoopError::Config(format!(
                        "{}: inconsistent credential storage metadata for registration {}",
                        path.display(),
                        registration.id
                    )));
                }
            }
        }
        if self.active_registration_id.as_ref().is_some_and(|active| {
            !self
                .registrations
                .iter()
                .any(|registration| &registration.id == active)
        }) {
            return Err(SpecLoopError::Config(format!(
                "{}: active ChatGPT registration does not exist",
                path.display()
            )));
        }
        Ok(())
    }

    fn empty() -> Self {
        Self {
            version: 1,
            host_id: format!("urn:uuid:{}", Uuid::new_v4()),
            registrations: Vec::new(),
            active_registration_id: None,
        }
    }

    fn load(path: &Path) -> Result<Self> {
        match fs::read(path) {
            Ok(bytes) => {
                let mut registry: Self = serde_json::from_slice(&bytes).map_err(|error| {
                    SpecLoopError::Config(format!(
                        "{}: invalid authentication registry: {error}",
                        path.display()
                    ))
                })?;
                if registry.version != 1 {
                    return Err(SpecLoopError::Config(format!(
                        "{}: unsupported authentication registry version {}",
                        path.display(),
                        registry.version
                    )));
                }
                if let Some(host_id) = normalize_host_id(&registry.host_id) {
                    registry.host_id = host_id;
                }
                registry.validate(path)?;
                Ok(registry)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::empty()),
            Err(source) => Err(io_error("read authentication registry", path, source)),
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        self.validate(path)?;
        let parent = path.parent().ok_or_else(|| {
            SpecLoopError::Config(format!(
                "authentication path has no parent: {}",
                path.display()
            ))
        })?;
        fs::create_dir_all(parent)
            .map_err(|source| io_error("create authentication directory", parent, source))?;
        restrict_directory(parent)?;
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| {
            SpecLoopError::Config(format!("serialize authentication registry: {error}"))
        })?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|source| {
            io_error("create temporary authentication registry", parent, source)
        })?;
        restrict_file(temporary.path())?;
        temporary
            .write_all(&bytes)
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|source| io_error("write authentication registry", path, source))?;
        temporary
            .persist(path)
            .map_err(|error| io_error("replace authentication registry", path, error.error))?;
        restrict_file(path)?;
        Ok(())
    }
}

fn normalize_host_id(host_id: &str) -> Option<String> {
    let uuid_text = host_id.strip_prefix("urn:uuid:").unwrap_or(host_id);
    let uuid = Uuid::parse_str(uuid_text).ok()?;
    if uuid.get_version_num() != 4 || uuid.hyphenated().to_string() != uuid_text {
        return None;
    }
    Some(format!("urn:uuid:{uuid}"))
}

fn restrict_directory(path: &Path) -> Result<()> {
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

fn restrict_file(path: &Path) -> Result<()> {
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

fn registry_path() -> Result<PathBuf> {
    let current_dir = env::current_dir().map_err(|source| {
        io_error(
            "resolve current directory for authentication storage",
            ".",
            source,
        )
    })?;
    let project_root = project_boundary(&current_dir)?;
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

fn project_boundary(current_dir: &Path) -> Result<Option<PathBuf>> {
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

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn registry_path_for_config_dir(config_dir: &Path, project_root: Option<&Path>) -> Result<PathBuf> {
    validate_external_registry_path(&config_dir.join("spec-loop/auth.json"), project_root)
}

fn validate_external_registry_path(path: &Path, project_root: Option<&Path>) -> Result<PathBuf> {
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

/// Dispatches profile-independent authentication commands.
pub fn run(command: AuthCommand, cancelled: &Arc<AtomicBool>) -> Result<()> {
    let path = registry_path()?;
    match command {
        AuthCommand::Login {
            account,
            new_account,
        } => {
            runtime::block_on(login(path, account, new_account, Arc::clone(cancelled)))??;
        }
        AuthCommand::Status => status(&path)?,
        AuthCommand::Logout { account } => logout(&path, account.as_deref())?,
    }
    Ok(())
}

/// Returns active `ChatGPT` access token, refreshing and persisting it when necessary.
pub async fn active_access_token(force_refresh: bool, cancelled: &AtomicBool) -> Result<String> {
    let path = registry_path()?;
    active_access_token_at(&path, force_refresh, cancelled).await
}

/// Loads active token from explicit registry path, enforcing permission before refresh.
pub async fn active_access_token_at(
    path: &Path,
    force_refresh: bool,
    cancelled: &AtomicBool,
) -> Result<String> {
    let (mut registry, registration, credential) = load_active_credential(path)?;
    require_direct_permission(&credential)?;
    if !force_refresh && !refresh_is_due(&credential, unix_time_now()?) {
        return Ok(credential.access_token);
    }
    let updated = refresh_credential(&registration, &credential, cancelled).await?;
    if cancelled.load(Ordering::Relaxed) {
        return Err(SpecLoopError::Interrupted);
    }
    store_refreshed_credential(path, &mut registry, &registration, updated.clone())?;
    Ok(updated.access_token)
}

fn require_direct_permission(credential: &Credential) -> Result<()> {
    if credential.scopes.iter().any(|scope| scope == DIRECT_SCOPE) {
        return Ok(());
    }
    Err(SpecLoopError::Workflow(format!(
        "ChatGPT credentials lack `{DIRECT_SCOPE}` permission; run `spec-loop auth login`"
    )))
}

/// Writes isolated file-backed credentials for backend permission-gate tests.
#[cfg(test)]
pub fn write_test_credentials(path: &Path, scopes: Vec<String>) -> Result<()> {
    let issuer = ISSUER.to_owned();
    let subject = "builtin-permission-test".to_owned();
    let client_id = "fixture-client".to_owned();
    let id = registration_id(&issuer, &subject, &client_id)?;
    let mut registry = Registry::empty();
    registry.registrations.push(Registration {
        id: id.clone(),
        issuer,
        subject,
        client_id,
        credential_storage: CredentialStorage::File,
        fallback_credential: Some(Credential {
            access_token: "fixture-access-token".to_owned(),
            refresh_token: Some("fixture-refresh-token".to_owned()),
            id_token: "fixture-id-token".to_owned(),
            scopes,
            expires_at_unix: Some(u64::MAX),
        }),
    });
    registry.active_registration_id = Some(id);
    registry.save(path)
}

fn load_active_credential(path: &Path) -> Result<(Registry, Registration, Credential)> {
    let registry = Registry::load(path)?;
    let active_id = registry.active_registration_id.as_deref().ok_or_else(|| {
        SpecLoopError::Workflow(
            "ChatGPT credentials are missing; run `spec-loop auth login`".to_owned(),
        )
    })?;
    let registration = registry
        .registrations
        .iter()
        .find(|registration| registration.id == active_id)
        .cloned()
        .ok_or_else(|| {
            SpecLoopError::Config("active ChatGPT registration metadata is missing".to_owned())
        })?;
    let credential = load_credential(&registration)?.ok_or_else(|| {
        SpecLoopError::Workflow(
            "ChatGPT credentials are missing; run `spec-loop auth login`".to_owned(),
        )
    })?;
    Ok((registry, registration, credential))
}

async fn refresh_credential(
    registration: &Registration,
    credential: &Credential,
    cancelled: &AtomicBool,
) -> Result<Credential> {
    let refresh_token = credential.refresh_token.as_deref().ok_or_else(|| {
        SpecLoopError::Workflow(
            "ChatGPT access token expired and no refresh token is available; run `spec-loop auth login`"
                .to_owned(),
        )
    })?;
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| {
            SpecLoopError::Workflow(format!("construct OAuth HTTP client: {error}"))
        })?;
    let metadata = discover_provider(&registration.issuer, &http, cancelled).await?;
    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(registration.client_id.clone()),
        None,
    );
    let refreshed = cancellable(cancelled, async {
        client
            .exchange_refresh_token(&RefreshToken::new(refresh_token.to_owned()))
            .map_err(|_| {
                SpecLoopError::Workflow(
                    "could not prepare ChatGPT credential refresh; run `spec-loop auth login`"
                        .to_owned(),
                )
            })?
            .request_async(&http)
            .await
            .map_err(|_| {
                SpecLoopError::Workflow(
                    "ChatGPT credentials expired or were revoked; run `spec-loop auth login`"
                        .to_owned(),
                )
            })
    })
    .await?;
    let scopes = token_scopes(&refreshed);
    refreshed_credential(
        credential,
        refreshed.access_token().secret().to_owned(),
        refreshed
            .refresh_token()
            .map(|token| token.secret().to_owned()),
        scopes,
        refreshed.expires_in(),
    )
}

async fn discover_provider(
    issuer: &str,
    http: &reqwest::Client,
    cancelled: &AtomicBool,
) -> Result<CoreProviderMetadata> {
    cancellable(cancelled, async {
        CoreProviderMetadata::discover_async(
            IssuerUrl::new(issuer.to_owned()).map_err(|error| {
                SpecLoopError::Config(format!("invalid OpenAI issuer: {error}"))
            })?,
            http,
        )
        .await
        .map_err(|_| {
            SpecLoopError::Workflow(
                "OpenAI authorization-service discovery failed; check connectivity and retry `spec-loop auth login`"
                    .to_owned(),
            )
        })
    })
    .await
}

fn token_scopes(token: &CoreTokenResponse) -> Vec<String> {
    token
        .scopes()
        .map(|scopes| {
            scopes
                .iter()
                .map(|scope| scope.as_str().to_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

async fn login(
    path: PathBuf,
    account: Option<String>,
    new_account: bool,
    cancelled: Arc<AtomicBool>,
) -> Result<()> {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| {
            SpecLoopError::Workflow(format!("construct OAuth HTTP client: {error}"))
        })?;
    login_with_services(
        path,
        account,
        new_account,
        cancelled,
        LoginServices {
            issuer_url: ISSUER,
            http: &http,
            open_browser: webbrowser::open,
            receive_callback: await_callback,
            save_registration: persist_registration,
        },
    )
    .await
}

struct LoginServices<'a, B, C, P> {
    issuer_url: &'a str,
    http: &'a reqwest::Client,
    open_browser: B,
    receive_callback: C,
    save_registration: P,
}

async fn login_with_services<B, C, P, E>(
    path: PathBuf,
    account: Option<String>,
    new_account: bool,
    cancelled: Arc<AtomicBool>,
    services: LoginServices<'_, B, C, P>,
) -> Result<()>
where
    B: FnOnce(&str) -> std::result::Result<(), E>,
    C: FnOnce(TcpListener, u16, &Arc<AtomicBool>) -> Result<Callback>,
    P: FnOnce(&mut Registry, &Path, String, String, String, Credential, Option<&str>) -> Result<()>,
{
    let LoginServices {
        issuer_url,
        http,
        open_browser,
        receive_callback,
        save_registration,
    } = services;
    if cancelled.load(Ordering::Relaxed) {
        return Err(SpecLoopError::Interrupted);
    }
    let registry_exists = path
        .try_exists()
        .map_err(|source| io_error("inspect authentication registry", &path, source))?;
    let mut registry = Registry::load(&path)?;
    let existing = select_registration(&registry, account.as_deref(), new_account)?.cloned();
    let client_id = existing
        .as_ref()
        .map_or("dynamic_agent_client", |entry| entry.client_id.as_str());
    let existing_id = existing
        .as_ref()
        .map(|registration| registration.id.clone());
    let existing_credential = existing
        .as_ref()
        .map(load_credential)
        .transpose()?
        .flatten();
    if !registry_exists {
        registry.save(&path)?;
    }
    let (redirect_listener, port, redirect_url) = callback_listener()?;
    let provider = discover_provider(issuer_url, http, &cancelled).await?;
    let client = CoreClient::from_provider_metadata(
        provider.clone(),
        ClientId::new(client_id.to_owned()),
        None,
    )
    .set_redirect_uri(redirect_url.clone());
    let authorization_registration = match existing.as_ref() {
        Some(_) => AuthorizationRegistration::Returning {
            id_token_hint: existing_credential
                .as_ref()
                .map(|credential| credential.id_token.as_str()),
        },
        None => AuthorizationRegistration::New,
    };
    let authorization =
        authorization_details(&client, &registry.host_id, authorization_registration);
    present_authorization_url(&authorization.url, open_browser, |message| match message {
        BrowserMessage::AuthorizationUrl(url) => {
            println!("Open this authorization URL: {url}");
        }
        BrowserMessage::ManualOpenFallback => {
            eprintln!("Could not open browser; open the URL above manually.");
        }
    });

    let callback = receive_callback(redirect_listener, port, &cancelled)?;
    let (authorization_code, issued_client_id) =
        validate_callback(callback, authorization.state.secret(), existing.as_ref())?;
    let token_client =
        CoreClient::from_provider_metadata(provider, ClientId::new(issued_client_id.clone()), None)
            .set_redirect_uri(redirect_url);
    let token = exchange_authorization_code(
        &token_client,
        authorization_code,
        authorization.verifier,
        http,
        &cancelled,
    )
    .await?;
    let verifier = token_client.id_token_verifier();
    let (issuer, subject, credential) =
        credential_from_token(&token, &verifier, &authorization.nonce)?;
    if let Some(previous) = &existing
        && (previous.issuer != issuer || previous.subject != subject)
    {
        return Err(SpecLoopError::Config(
            "ChatGPT identity differs from saved registration; existing credentials were preserved"
                .to_owned(),
        ));
    }
    if cancelled.load(Ordering::Relaxed) {
        return Err(SpecLoopError::Interrupted);
    }
    save_registration(
        &mut registry,
        &path,
        issuer,
        subject,
        issued_client_id,
        credential,
        existing_id.as_deref(),
    )?;
    println!("ChatGPT authorization saved. `spec-loop auth status` reports local state.");
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
enum BrowserMessage<'a> {
    AuthorizationUrl(&'a str),
    ManualOpenFallback,
}

fn present_authorization_url<E>(
    url: &Url,
    open_browser: impl FnOnce(&str) -> std::result::Result<(), E>,
    mut report: impl FnMut(BrowserMessage<'_>),
) {
    let manual_url = manual_authorization_url(url);
    report(BrowserMessage::AuthorizationUrl(manual_url.as_str()));
    if open_browser(manual_url.as_str()).is_err() {
        report(BrowserMessage::ManualOpenFallback);
    }
}

fn manual_authorization_url(url: &Url) -> Url {
    let safe_parameters = url
        .query_pairs()
        .filter(|(name, _)| name != "id_token_hint")
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    let mut manual_url = url.clone();
    manual_url
        .query_pairs_mut()
        .clear()
        .extend_pairs(safe_parameters);
    manual_url
}

struct AuthorizationDetails {
    url: Url,
    state: CsrfToken,
    nonce: Nonce,
    verifier: PkceCodeVerifier,
}

fn callback_listener() -> Result<(TcpListener, u16, RedirectUrl)> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|source| io_error("bind OAuth callback listener", "127.0.0.1", source))?;
    listener
        .set_nonblocking(true)
        .map_err(|source| io_error("configure OAuth callback listener", "127.0.0.1", source))?;
    let port = listener
        .local_addr()
        .map_err(|source| io_error("inspect OAuth callback listener", "127.0.0.1", source))?
        .port();
    let redirect = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let redirect_url = RedirectUrl::new(redirect)
        .map_err(|error| SpecLoopError::Config(format!("invalid OAuth callback URL: {error}")))?;
    Ok((listener, port, redirect_url))
}

#[derive(Clone, Copy)]
enum AuthorizationRegistration<'a> {
    New,
    Returning { id_token_hint: Option<&'a str> },
}

fn authorization_details(
    client: &ConfiguredCoreClient,
    host_id: &str,
    registration: AuthorizationRegistration<'_>,
) -> AuthorizationDetails {
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let mut request = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("openid".to_owned()))
        .add_scope(Scope::new("profile".to_owned()))
        .add_scope(Scope::new("email".to_owned()))
        .add_scope(Scope::new("offline_access".to_owned()))
        .add_scope(Scope::new("resource.invoke".to_owned()))
        .add_scope(Scope::new(DIRECT_SCOPE.to_owned()))
        .add_extra_param("resource", "https://api.openai.com/v1")
        .add_extra_param("ext_agent_host_id", host_id);
    match registration {
        AuthorizationRegistration::New => {
            request = request.add_extra_param("agent_name_hint", "Spec Loop");
        }
        AuthorizationRegistration::Returning { id_token_hint } => {
            if let Some(id_token_hint) = id_token_hint {
                request = request.add_extra_param("id_token_hint", id_token_hint);
            }
        }
    }
    let request = request.set_pkce_challenge(challenge);
    let (url, state, nonce) = request.url();
    AuthorizationDetails {
        url,
        state,
        nonce,
        verifier,
    }
}

async fn exchange_authorization_code(
    client: &ConfiguredCoreClient,
    code: AuthorizationCode,
    verifier: PkceCodeVerifier,
    http: &reqwest::Client,
    cancelled: &AtomicBool,
) -> Result<CoreTokenResponse> {
    cancellable(cancelled, async {
        client
            .exchange_code(code)
            .map_err(|error| {
                SpecLoopError::Workflow(format!("prepare OAuth token exchange: {error}"))
            })?
            .set_pkce_verifier(verifier)
            .add_extra_param("resource", "https://api.openai.com/v1")
            .request_async(http)
            .await
            .map_err(|_| {
                SpecLoopError::Workflow(
                    "OpenAI token exchange failed; run `spec-loop auth login` again".to_owned(),
                )
            })
    })
    .await
}

fn refresh_is_due(credential: &Credential, now_unix: u64) -> bool {
    credential
        .expires_at_unix
        .is_none_or(|expires_at| expires_at <= now_unix.saturating_add(60))
}

fn unix_time_now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| {
            SpecLoopError::Workflow(format!(
                "read system time for ChatGPT token expiry: {error}"
            ))
        })
}

fn refreshed_credential(
    current: &Credential,
    access_token: String,
    refresh_token: Option<String>,
    scopes: Vec<String>,
    expires_in: Option<Duration>,
) -> Result<Credential> {
    if !scopes.iter().any(|scope| scope == DIRECT_SCOPE) {
        return Err(SpecLoopError::Workflow(format!(
            "refreshed ChatGPT token lacks `{DIRECT_SCOPE}` permission; run `spec-loop auth login`"
        )));
    }
    let expires_at_unix = Some(expiry_timestamp(
        expires_in,
        "refreshed ChatGPT token response omitted or exceeded representable expiry; run `spec-loop auth login`",
    )?);
    let credential = Credential {
        access_token,
        refresh_token: refresh_token.or_else(|| current.refresh_token.clone()),
        id_token: current.id_token.clone(),
        scopes,
        expires_at_unix,
    };
    validate_credential(&credential)?;
    Ok(credential)
}

fn store_refreshed_credential(
    path: &Path,
    registry: &mut Registry,
    registration: &Registration,
    credential: Credential,
) -> Result<()> {
    match registration.credential_storage {
        CredentialStorage::Keyring => save_credential(&registration.id, &credential),
        CredentialStorage::File => {
            let selected = registry
                .registrations
                .iter_mut()
                .find(|entry| entry.id == registration.id)
                .ok_or_else(|| {
                    SpecLoopError::Config(
                        "ChatGPT registration disappeared before credential refresh".to_owned(),
                    )
                })?;
            selected.fallback_credential = Some(credential);
            registry.save(path)
        }
        CredentialStorage::LoggedOut => Err(SpecLoopError::Workflow(
            "ChatGPT credentials were logged out; run `spec-loop auth login`".to_owned(),
        )),
    }
}

async fn cancellable<T>(
    cancelled: &AtomicBool,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(SpecLoopError::Interrupted);
    }
    tokio::select! {
        biased;
        () = wait_for_cancellation(cancelled) => Err(SpecLoopError::Interrupted),
        result = future => {
            if cancelled.load(Ordering::Relaxed) {
                Err(SpecLoopError::Interrupted)
            } else {
                result
            }
        }
    }
}

async fn wait_for_cancellation(cancelled: &AtomicBool) {
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn validate_callback(
    callback: Callback,
    expected_state: &str,
    existing: Option<&Registration>,
) -> Result<(AuthorizationCode, String)> {
    if callback.state != expected_state {
        return Err(SpecLoopError::Config(
            "OAuth callback state validation failed".to_owned(),
        ));
    }
    if callback.error.is_some() {
        return Err(SpecLoopError::Workflow(
            "OpenAI authorization was denied or failed; run `spec-loop auth login` again"
                .to_owned(),
        ));
    }
    let authorization_code = callback.code.ok_or_else(|| {
        SpecLoopError::Config("OAuth callback did not contain an authorization code".to_owned())
    })?;
    let issued_client_id = match (existing, callback.client_id) {
        (Some(registration), Some(supplied)) if supplied != registration.client_id => {
            return Err(SpecLoopError::Config(
                "OAuth callback client ID does not match selected registration; run `spec-loop auth login` again".to_owned(),
            ));
        }
        (Some(registration), _) => registration.client_id.clone(),
        (None, Some(supplied)) if !supplied.trim().is_empty() => supplied,
        (None, _) => {
            return Err(SpecLoopError::Config(
                "OpenAI authorization callback omitted the dynamically issued client ID".to_owned(),
            ));
        }
    };
    Ok((AuthorizationCode::new(authorization_code), issued_client_id))
}

fn select_registration<'a>(
    registry: &'a Registry,
    account: Option<&str>,
    new_account: bool,
) -> Result<Option<&'a Registration>> {
    if new_account && account.is_some() {
        return Err(SpecLoopError::Config(
            "--account and --new-account cannot be used together".to_owned(),
        ));
    }
    let selected_id = if new_account {
        None
    } else if let Some(account) = account {
        Some(account)
    } else {
        match registry.registrations.as_slice() {
            [] => None,
            [registration] => Some(registration.id.as_str()),
            _ => {
                return Err(SpecLoopError::Config(
                    "multiple ChatGPT registrations exist; select `--account ID` or use `--new-account`".to_owned(),
                ));
            }
        }
    };
    selected_id
        .map(|id| {
            registry
                .registrations
                .iter()
                .find(|registration| registration.id == id)
                .ok_or_else(|| {
                    SpecLoopError::Config(format!("unknown ChatGPT registration ID {id:?}"))
                })
        })
        .transpose()
}

struct RegistrationUpdate {
    registration: Registration,
    credential: Credential,
    selected_id: Option<String>,
}

struct RegistrationTransaction {
    registration_id: String,
    original: Registry,
    staged: Registry,
    previous_keyring_credential: Option<Credential>,
    is_new_registration: bool,
}

struct RegistrationPersistence<SaveRegistry, WriteCredential, ReadCredential, RestoreCredential> {
    save_registry: SaveRegistry,
    write_credential: WriteCredential,
    read_credential: ReadCredential,
    restore_credential: RestoreCredential,
}

fn persist_registration(
    registry: &mut Registry,
    path: &Path,
    issuer: String,
    subject: String,
    client_id: String,
    credential: Credential,
    selected_id: Option<&str>,
) -> Result<()> {
    let id = registration_id(&issuer, &subject, &client_id)?;
    persist_registration_with(
        registry,
        path,
        RegistrationUpdate {
            registration: Registration {
                id,
                issuer,
                subject,
                client_id,
                credential_storage: CredentialStorage::Keyring,
                fallback_credential: None,
            },
            credential,
            selected_id: selected_id.map(str::to_owned),
        },
        RegistrationPersistence {
            save_registry: Registry::save,
            write_credential: try_save_credential,
            read_credential: load_credential,
            restore_credential: |registration_id: &str, previous: Option<&Credential>| {
                previous.map_or_else(
                    || delete_credential(registration_id),
                    |credential| save_credential(registration_id, credential),
                )
            },
        },
    )
}

fn persist_registration_with<SaveRegistry, WriteCredential, ReadCredential, RestoreCredential>(
    registry: &mut Registry,
    path: &Path,
    update: RegistrationUpdate,
    mut persistence: RegistrationPersistence<
        SaveRegistry,
        WriteCredential,
        ReadCredential,
        RestoreCredential,
    >,
) -> Result<()>
where
    SaveRegistry: FnMut(&Registry, &Path) -> Result<()>,
    WriteCredential: FnMut(&str, &Credential) -> std::result::Result<(), CredentialWriteError>,
    ReadCredential: FnMut(&Registration) -> Result<Option<Credential>>,
    RestoreCredential: FnMut(&str, Option<&Credential>) -> Result<()>,
{
    let id = &update.registration.id;
    if update
        .selected_id
        .as_deref()
        .is_some_and(|selected| selected != id)
    {
        return Err(SpecLoopError::Config(
            "verified ChatGPT identity does not match selected registration; existing credentials were preserved".to_owned(),
        ));
    }
    let previous = registry
        .registrations
        .iter()
        .find(|previous| previous.id == *id)
        .cloned();
    if update.selected_id.is_none() && previous.is_some() {
        return Err(SpecLoopError::Config(
            "this ChatGPT identity and client registration already exists; select its registration ID".to_owned(),
        ));
    }
    let previous_keyring_credential = match previous.as_ref() {
        Some(entry) if matches!(entry.credential_storage, CredentialStorage::Keyring) => {
            (persistence.read_credential)(entry)?
        }
        _ => None,
    };
    let original = registry.clone();
    let is_new_registration = previous.is_none();
    let mut staged = original.clone();
    if is_new_registration {
        staged.registrations.push(update.registration.clone());
    }
    let transaction = RegistrationTransaction {
        registration_id: id.clone(),
        original,
        staged,
        previous_keyring_credential,
        is_new_registration,
    };

    // Persist identity-bound metadata before writing credentials; it stays inactive until commit.
    (persistence.save_registry)(&transaction.staged, path)?;
    match (persistence.write_credential)(id, &update.credential) {
        Ok(()) => {
            commit_keyring_registration(registry, path, update, &transaction, &mut persistence)
        }
        Err(CredentialWriteError::Unavailable) => {
            commit_file_registration(registry, path, update, &transaction, &mut persistence)
        }
        Err(CredentialWriteError::Failed(error)) => Err(rollback_registration(
            error,
            path,
            &transaction,
            &mut persistence,
            true,
        )),
    }
}

fn commit_keyring_registration<SaveRegistry, WriteCredential, ReadCredential, RestoreCredential>(
    registry: &mut Registry,
    path: &Path,
    update: RegistrationUpdate,
    transaction: &RegistrationTransaction,
    persistence: &mut RegistrationPersistence<
        SaveRegistry,
        WriteCredential,
        ReadCredential,
        RestoreCredential,
    >,
) -> Result<()>
where
    SaveRegistry: FnMut(&Registry, &Path) -> Result<()>,
    WriteCredential: FnMut(&str, &Credential) -> std::result::Result<(), CredentialWriteError>,
    ReadCredential: FnMut(&Registration) -> Result<Option<Credential>>,
    RestoreCredential: FnMut(&str, Option<&Credential>) -> Result<()>,
{
    let mut activated = transaction.staged.clone();
    replace_registration(&mut activated, update.registration);
    activated.active_registration_id = Some(transaction.registration_id.clone());
    save_activation(registry, path, activated, transaction, persistence, true)
}

fn commit_file_registration<SaveRegistry, WriteCredential, ReadCredential, RestoreCredential>(
    registry: &mut Registry,
    path: &Path,
    mut update: RegistrationUpdate,
    transaction: &RegistrationTransaction,
    persistence: &mut RegistrationPersistence<
        SaveRegistry,
        WriteCredential,
        ReadCredential,
        RestoreCredential,
    >,
) -> Result<()>
where
    SaveRegistry: FnMut(&Registry, &Path) -> Result<()>,
    WriteCredential: FnMut(&str, &Credential) -> std::result::Result<(), CredentialWriteError>,
    ReadCredential: FnMut(&Registration) -> Result<Option<Credential>>,
    RestoreCredential: FnMut(&str, Option<&Credential>) -> Result<()>,
{
    update.registration.credential_storage = CredentialStorage::File;
    update.registration.fallback_credential = Some(update.credential);
    let mut activated = transaction.staged.clone();
    replace_registration(&mut activated, update.registration);
    activated.active_registration_id = Some(transaction.registration_id.clone());
    save_activation(registry, path, activated, transaction, persistence, false)?;
    println!(
        "OS credential store unavailable; credentials saved unencrypted with owner-only permissions at {}.",
        path.display()
    );
    Ok(())
}

fn save_activation<SaveRegistry, WriteCredential, ReadCredential, RestoreCredential>(
    registry: &mut Registry,
    path: &Path,
    activated: Registry,
    transaction: &RegistrationTransaction,
    persistence: &mut RegistrationPersistence<
        SaveRegistry,
        WriteCredential,
        ReadCredential,
        RestoreCredential,
    >,
    restore_keyring: bool,
) -> Result<()>
where
    SaveRegistry: FnMut(&Registry, &Path) -> Result<()>,
    WriteCredential: FnMut(&str, &Credential) -> std::result::Result<(), CredentialWriteError>,
    ReadCredential: FnMut(&Registration) -> Result<Option<Credential>>,
    RestoreCredential: FnMut(&str, Option<&Credential>) -> Result<()>,
{
    if let Err(error) = (persistence.save_registry)(&activated, path) {
        return Err(rollback_registration(
            error,
            path,
            transaction,
            persistence,
            restore_keyring,
        ));
    }
    *registry = activated;
    Ok(())
}

fn rollback_registration<SaveRegistry, WriteCredential, ReadCredential, RestoreCredential>(
    error: SpecLoopError,
    path: &Path,
    transaction: &RegistrationTransaction,
    persistence: &mut RegistrationPersistence<
        SaveRegistry,
        WriteCredential,
        ReadCredential,
        RestoreCredential,
    >,
    restore_keyring: bool,
) -> SpecLoopError
where
    SaveRegistry: FnMut(&Registry, &Path) -> Result<()>,
    WriteCredential: FnMut(&str, &Credential) -> std::result::Result<(), CredentialWriteError>,
    ReadCredential: FnMut(&Registration) -> Result<Option<Credential>>,
    RestoreCredential: FnMut(&str, Option<&Credential>) -> Result<()>,
{
    let credential_rollback = if restore_keyring {
        (persistence.restore_credential)(
            &transaction.registration_id,
            transaction.previous_keyring_credential.as_ref(),
        )
    } else {
        Ok(())
    };
    let registry_rollback = if transaction.is_new_registration {
        (persistence.save_registry)(&transaction.original, path)
    } else {
        Ok(())
    };
    combine_registration_rollback(error, credential_rollback, registry_rollback)
}

fn replace_registration(registry: &mut Registry, registration: Registration) {
    if let Some(previous) = registry
        .registrations
        .iter_mut()
        .find(|previous| previous.id == registration.id)
    {
        *previous = registration;
    } else {
        registry.registrations.push(registration);
    }
}

fn combine_registration_rollback(
    original: SpecLoopError,
    credential_rollback: Result<()>,
    registry_rollback: Result<()>,
) -> SpecLoopError {
    match (credential_rollback, registry_rollback) {
        (Ok(()), Ok(())) => original,
        (Err(credential_error), Ok(())) => SpecLoopError::Workflow(format!(
            "registration persistence failed: {original}; restoring prior credential also failed: {credential_error}"
        )),
        (Ok(()), Err(registry_error)) => SpecLoopError::Workflow(format!(
            "registration persistence failed: {original}; restoring prior registry metadata also failed: {registry_error}"
        )),
        (Err(credential_error), Err(registry_error)) => SpecLoopError::Workflow(format!(
            "registration persistence failed: {original}; credential rollback failed: {credential_error}; registry rollback failed: {registry_error}"
        )),
    }
}

fn credential_from_token(
    token: &CoreTokenResponse,
    verifier: &CoreIdTokenVerifier<'_>,
    nonce: &Nonce,
) -> Result<(String, String, Credential)> {
    let scopes = token
        .scopes()
        .map(|scopes| {
            scopes
                .iter()
                .map(|scope| scope.as_str().to_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !scopes.iter().any(|scope| scope == DIRECT_SCOPE) {
        return Err(SpecLoopError::Workflow(format!(
            "OpenAI authorization lacks required `{DIRECT_SCOPE}` permission; grant it and run `spec-loop auth login` again"
        )));
    }
    let id_token = token.id_token().ok_or_else(|| {
        SpecLoopError::Workflow("OpenAI token response omitted the required ID token".to_owned())
    })?;
    let claims = id_token.claims(verifier, nonce).map_err(|_| {
        SpecLoopError::Workflow(
            "OpenAI ID token validation failed; run `spec-loop auth login` again".to_owned(),
        )
    })?;
    let issuer = claims.issuer().as_str().to_owned();
    let subject = claims.subject().as_str().to_owned();
    let expires_at_unix = Some(expiry_timestamp(
        token.expires_in(),
        "OpenAI token response omitted or exceeded representable expiry; run `spec-loop auth login` again",
    )?);
    let credential = Credential {
        access_token: token.access_token().secret().to_owned(),
        refresh_token: token
            .refresh_token()
            .map(|refresh| refresh.secret().to_owned()),
        id_token: id_token.to_string(),
        scopes,
        expires_at_unix,
    };
    validate_credential(&credential)?;
    Ok((issuer, subject, credential))
}

struct Callback {
    state: String,
    code: Option<String>,
    client_id: Option<String>,
    error: Option<String>,
}

fn await_callback(
    listener: TcpListener,
    port: u16,
    cancelled: &Arc<AtomicBool>,
) -> Result<Callback> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let deadline = Instant::now()
        .checked_add(CALLBACK_TIMEOUT)
        .ok_or_else(|| SpecLoopError::Config("OAuth callback deadline overflow".to_owned()))?;
    let worker_cancelled = Arc::clone(cancelled);
    let stop_worker = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop_worker);
    let worker = thread::Builder::new()
        .name("spec-loop-oauth-callback".to_owned())
        .spawn(move || {
            callback_worker(
                &listener,
                &sender,
                &worker_cancelled,
                &worker_stop,
                deadline,
            );
        })
        .map_err(|source| io_error("start OAuth callback worker", "127.0.0.1", source))?;
    loop {
        if worker.is_finished() {
            break;
        }
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(result) => {
                worker.join().map_err(|_| {
                    SpecLoopError::Workflow(
                        "OAuth callback worker terminated unexpectedly".to_owned(),
                    )
                })?;
                return result;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        if cancelled.load(Ordering::Relaxed) {
            stop_worker.store(true, Ordering::Relaxed);
            worker.join().map_err(|_| {
                SpecLoopError::Workflow("OAuth callback worker terminated unexpectedly".to_owned())
            })?;
            return Err(SpecLoopError::Interrupted);
        }
        if Instant::now() >= deadline {
            stop_worker.store(true, Ordering::Relaxed);
            worker.join().map_err(|_| {
                SpecLoopError::Workflow("OAuth callback worker terminated unexpectedly".to_owned())
            })?;
            return Err(SpecLoopError::Timeout(format!(
                "OAuth callback on 127.0.0.1:{port} expired after five minutes"
            )));
        }
    }
    worker.join().map_err(|_| {
        SpecLoopError::Workflow("OAuth callback worker terminated unexpectedly".to_owned())
    })?;
    receiver
        .recv()
        .map_err(|_| SpecLoopError::Workflow("OAuth callback ended without a result".to_owned()))?
}

fn callback_worker(
    listener: &TcpListener,
    sender: &std::sync::mpsc::Sender<Result<Callback>>,
    cancelled: &AtomicBool,
    stop: &AtomicBool,
    deadline: Instant,
) {
    let result = loop {
        if cancelled.load(Ordering::Relaxed) {
            break Err(SpecLoopError::Interrupted);
        }
        if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
            break Err(SpecLoopError::Timeout(
                "OAuth callback listener stopped".to_owned(),
            ));
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                if let Err(source) = stream.set_read_timeout(Some(Duration::from_millis(50))) {
                    break Err(io_error(
                        "configure OAuth callback stream",
                        "127.0.0.1",
                        source,
                    ));
                }
                break parse_callback_interruptible(&mut stream, cancelled, stop, Some(deadline));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(source) => break Err(io_error("accept OAuth callback", "127.0.0.1", source)),
        }
    };
    if let Err(error) = sender.send(result)
        && let Err(send_error) = error.0
    {
        eprintln!("spec-loop OAuth callback failed: {send_error}");
    }
}

#[cfg(test)]
fn parse_callback(stream: &mut TcpStream) -> Result<Callback> {
    let never_cancelled = AtomicBool::new(false);
    parse_callback_interruptible(stream, &never_cancelled, &never_cancelled, None)
}

fn parse_callback_interruptible(
    stream: &mut TcpStream,
    cancelled: &AtomicBool,
    stop: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<Callback> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(SpecLoopError::Interrupted);
        }
        if stop.load(Ordering::Relaxed) {
            return Err(SpecLoopError::Timeout(
                "OAuth callback listener stopped".to_owned(),
            ));
        }
        if deadline.is_some_and(|limit| Instant::now() >= limit) {
            return Err(SpecLoopError::Timeout(
                "OAuth callback listener stopped".to_owned(),
            ));
        }
        let read = match stream.read(&mut buffer) {
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(source) => return Err(io_error("read OAuth callback", "127.0.0.1", source)),
        };
        if read == 0 {
            if cancelled.load(Ordering::Relaxed) {
                return Err(SpecLoopError::Interrupted);
            }
            if stop.load(Ordering::Relaxed) {
                return Err(SpecLoopError::Timeout(
                    "OAuth callback listener stopped".to_owned(),
                ));
            }
            return Err(SpecLoopError::Config(
                "OAuth callback ended before HTTP headers completed".to_owned(),
            ));
        }
        request.try_reserve(read).map_err(|error| {
            SpecLoopError::Workflow(format!("retain OAuth callback headers: {error}"))
        })?;
        request.extend_from_slice(&buffer[..read]);
        if request.len() > MAX_CALLBACK_BYTES {
            return Err(SpecLoopError::Config(
                "OAuth callback headers exceeded 16 KiB".to_owned(),
            ));
        }
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let callback = parse_callback_request(&request)?;
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\nAuthorization received. Return to Spec Loop.")
        .map_err(|source| io_error("respond to OAuth callback", "127.0.0.1", source))?;
    Ok(callback)
}

fn parse_callback_request(request: &[u8]) -> Result<Callback> {
    let header_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| SpecLoopError::Config("OAuth callback headers are incomplete".to_owned()))?;
    let body_start = header_end.checked_add(4).ok_or_else(|| {
        SpecLoopError::Config("OAuth callback header length overflowed".to_owned())
    })?;
    if body_start != request.len() {
        return Err(SpecLoopError::Config(
            "OAuth callback request body is not allowed".to_owned(),
        ));
    }
    let text = std::str::from_utf8(&request[..header_end]).map_err(|error| {
        SpecLoopError::Config(format!("OAuth callback headers are not UTF-8: {error}"))
    })?;
    let Some(line) = text.lines().next() else {
        return Err(SpecLoopError::Config(
            "OAuth callback request line is missing".to_owned(),
        ));
    };
    for header in text.lines().skip(1) {
        let (name, value) = header.split_once(':').ok_or_else(|| {
            SpecLoopError::Config("OAuth callback contains malformed HTTP header".to_owned())
        })?;
        if name.eq_ignore_ascii_case("content-length") && value.trim() != "0"
            || name.eq_ignore_ascii_case("transfer-encoding")
        {
            return Err(SpecLoopError::Config(
                "OAuth callback request body is not allowed".to_owned(),
            ));
        }
    }
    let mut parts = line.split_ascii_whitespace();
    let method = parts.next();
    let target = parts.next();
    let version = parts.next();
    if method != Some("GET") || version != Some("HTTP/1.1") || parts.next().is_some() {
        return Err(SpecLoopError::Config(
            "OAuth callback must use a bodyless HTTP/1.1 GET request".to_owned(),
        ));
    }
    let target = target
        .ok_or_else(|| SpecLoopError::Config("OAuth callback target is missing".to_owned()))?;
    let callback_url = Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|error| SpecLoopError::Config(format!("invalid OAuth callback URL: {error}")))?;
    parse_callback_parameters(&callback_url)
}

fn parse_callback_parameters(callback_url: &Url) -> Result<Callback> {
    if callback_url.path() != CALLBACK_PATH {
        return Err(SpecLoopError::Config(
            "OAuth callback path did not match expected endpoint".to_owned(),
        ));
    }
    if callback_url.query().is_none() {
        return Err(SpecLoopError::Config(
            "OAuth callback query is missing".to_owned(),
        ));
    }
    let mut query = std::collections::HashMap::new();
    for (name, value) in callback_url.query_pairs() {
        if query.insert(name.to_string(), value.to_string()).is_some() {
            return Err(SpecLoopError::Config(
                "OAuth callback contains duplicate parameters".to_owned(),
            ));
        }
    }
    let state = query
        .remove("state")
        .ok_or_else(|| SpecLoopError::Config("OAuth callback state is missing".to_owned()))?;
    let code = query.remove("code");
    let client_id = query.remove("client_id");
    let error = query.remove("error");
    // OpenAI may echo granted scopes; the token response remains authoritative.
    let _callback_scope = query.remove("scope");
    if !query.is_empty() || code.is_some() == error.is_some() {
        return Err(SpecLoopError::Config(
            "OAuth callback parameters are invalid".to_owned(),
        ));
    }
    Ok(Callback {
        state,
        code,
        client_id,
        error,
    })
}

fn expiry_timestamp(expires_in: Option<Duration>, guidance: &str) -> Result<u64> {
    let Some(lifetime) = expires_in else {
        return Err(SpecLoopError::Workflow(guidance.to_owned()));
    };
    let expires_at = SystemTime::now()
        .checked_add(lifetime)
        .ok_or_else(|| SpecLoopError::Workflow(guidance.to_owned()))?
        .duration_since(UNIX_EPOCH)
        .map_err(|_| SpecLoopError::Workflow(guidance.to_owned()))?
        .as_secs();
    Ok(expires_at)
}

fn validate_credential(credential: &Credential) -> Result<()> {
    if credential.access_token.is_empty()
        || credential.id_token.is_empty()
        || credential.expires_at_unix.is_none()
        || credential
            .refresh_token
            .as_ref()
            .is_some_and(String::is_empty)
        || credential
            .scopes
            .iter()
            .any(|scope| scope.is_empty() || scope.contains('\0'))
    {
        return Err(SpecLoopError::Config(
            "stored ChatGPT credential is malformed; run `spec-loop auth login` again".to_owned(),
        ));
    }
    Ok(())
}

fn status(path: &Path) -> Result<()> {
    let registry = Registry::load(path)?;
    if registry.registrations.is_empty() {
        println!("ChatGPT authorization: not configured");
        println!("Run `spec-loop auth login` to authorize ChatGPT.");
        return Ok(());
    }
    for registration in &registry.registrations {
        let active = registry.active_registration_id.as_deref() == Some(&registration.id);
        let credential = load_credential(registration)?;
        let state = if credential.is_some() {
            "locally stored"
        } else {
            "logged out locally"
        };
        println!(
            "Registration {}: {state}{}",
            registration.id,
            if active { " (active)" } else { "" }
        );
        if let Some(credential) = credential {
            let permission = credential.scopes.iter().any(|scope| scope == DIRECT_SCOPE);
            let expiry = credential
                .expires_at_unix
                .map_or_else(|| "unknown".to_owned(), |value| value.to_string());
            println!(
                "  Direct ChatGPT-plan permission: {}",
                if permission {
                    "granted locally"
                } else {
                    "missing"
                }
            );
            println!("  Access-token expiry (Unix seconds): {expiry}");
        }
    }
    println!("Remote revocation is not checked by status.");
    Ok(())
}

fn logout(path: &Path, requested_id: Option<&str>) -> Result<()> {
    let mut registry = Registry::load(path)?;
    if registry.registrations.is_empty() {
        println!("No Spec Loop ChatGPT credentials are stored.");
        return Ok(());
    }
    let registration_id = match requested_id {
        Some(id) => id.to_owned(),
        None => registry.active_registration_id.clone().ok_or_else(|| {
            SpecLoopError::Config(
                "no active ChatGPT registration; provide `--account ID`".to_owned(),
            )
        })?,
    };
    let Some(registration) = registry
        .registrations
        .iter_mut()
        .find(|registration| registration.id == registration_id)
    else {
        return Err(SpecLoopError::Config(format!(
            "unknown ChatGPT registration ID {registration_id:?}"
        )));
    };
    let storage = registration.credential_storage;
    registration.credential_storage = CredentialStorage::LoggedOut;
    registration.fallback_credential = None;
    if registry.active_registration_id.as_deref() == Some(registration_id.as_str()) {
        registry.active_registration_id = None;
    }
    registry.save(path)?;
    if matches!(storage, CredentialStorage::Keyring) {
        delete_credential(&registration_id)?;
    }
    println!(
        "Spec Loop ChatGPT credentials removed locally; upstream authorization was not revoked."
    );
    Ok(())
}

fn load_credential(registration: &Registration) -> Result<Option<Credential>> {
    match registration.credential_storage {
        CredentialStorage::Keyring => match keyring_entry(&registration.id)?.get_password() {
            Ok(secret) => {
                let credential: Credential = serde_json::from_str(&secret).map_err(|_| {
                    SpecLoopError::Config(
                        "stored ChatGPT credential is malformed; run `spec-loop auth login` again"
                            .to_owned(),
                    )
                })?;
                validate_credential(&credential)?;
                Ok(Some(credential))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(SpecLoopError::Workflow(format!(
                "read ChatGPT credential store: {error}"
            ))),
        },
        CredentialStorage::File => Ok(registration.fallback_credential.clone()),
        CredentialStorage::LoggedOut => Ok(None),
    }
}

fn save_credential(registration_id: &str, credential: &Credential) -> Result<()> {
    try_save_credential(registration_id, credential).map_err(CredentialWriteError::into_error)
}

fn try_save_credential(
    registration_id: &str,
    credential: &Credential,
) -> std::result::Result<(), CredentialWriteError> {
    let serialized = serde_json::to_string(credential).map_err(|error| {
        CredentialWriteError::Failed(SpecLoopError::Workflow(format!(
            "serialize ChatGPT credential: {error}"
        )))
    })?;
    let entry = keyring::Entry::new(KEYRING_SERVICE, registration_id).map_err(|error| {
        if credential_store_unavailable(&error) {
            CredentialWriteError::Unavailable
        } else {
            CredentialWriteError::Failed(SpecLoopError::Workflow(format!(
                "open operating-system credential store: {error}"
            )))
        }
    })?;
    entry.set_password(&serialized).map_err(|error| {
        CredentialWriteError::Failed(SpecLoopError::Workflow(format!(
            "write ChatGPT credential store: {error}"
        )))
    })
}

const fn credential_store_unavailable(error: &keyring::Error) -> bool {
    matches!(
        error,
        keyring::Error::NoDefaultStore | keyring::Error::NotSupportedByStore(_)
    )
}

fn delete_credential(registration_id: &str) -> Result<()> {
    match keyring_entry(registration_id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(SpecLoopError::Workflow(format!(
            "remove ChatGPT credential: {error}"
        ))),
    }
}

fn keyring_entry(registration_id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, registration_id).map_err(|error| {
        SpecLoopError::Workflow(format!("open operating-system credential store: {error}"))
    })
}

fn registration_id(issuer: &str, subject: &str, client_id: &str) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(issuer.as_bytes());
    digest.update([0]);
    digest.update(subject.as_bytes());
    digest.update([0]);
    digest.update(client_id.as_bytes());
    let bytes = digest.finalize();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(encoded, "{byte:02x}").map_err(|error| {
            SpecLoopError::Workflow(format!("encode authentication registration ID: {error}"))
        })?;
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::Path;
    #[cfg(windows)]
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use chrono::{Duration as ChronoDuration, Utc};
    use openidconnect::core::{
        CoreIdToken, CoreIdTokenClaims, CoreJsonWebKeySet, CoreJwsSigningAlgorithm,
        CoreRsaPrivateSigningKey,
    };
    use openidconnect::{
        AccessToken, Audience, EmptyAdditionalClaims, IssuerUrl, JsonWebKeyId, Nonce,
        PkceCodeChallenge, PkceCodeVerifier, PrivateSigningKey, StandardClaims, SubjectIdentifier,
    };
    use serde_json::json;
    use tempfile::TempDir;
    use url::Url;

    use super::{
        AuthorizationRegistration, BrowserMessage, CALLBACK_PATH, Callback, ConfiguredCoreClient,
        Credential, CredentialStorage, CredentialWriteError, DIRECT_SCOPE, LoginServices,
        Registration, RegistrationPersistence, RegistrationUpdate, Registry,
        active_access_token_at, authorization_details, await_callback, callback_listener,
        cancellable, credential_store_unavailable, expiry_timestamp, login_with_services,
        manual_authorization_url, parse_callback, persist_registration_with,
        present_authorization_url, project_boundary, refresh_credential, refresh_is_due,
        refreshed_credential, registration_id, registry_path_for_config_dir, validate_callback,
        validate_credential, validate_external_registry_path, write_test_credentials,
    };
    use crate::error::{Result, SpecLoopError};

    #[test]
    fn missing_direct_use_scope_rejects_saved_credentials_before_refresh() {
        let temporary = TempDir::new().expect("create credential fixture");
        let path = temporary.path().join("spec-loop/auth.json");
        write_test_credentials(&path, vec!["openid".to_owned()])
            .expect("write credential fixture without direct permission");

        let error = crate::runtime::block_on(active_access_token_at(
            &path,
            false,
            &AtomicBool::new(false),
        ))
        .expect("construct test runtime")
        .expect_err("missing direct-use permission must stop token admission");
        assert!(error.to_string().contains(DIRECT_SCOPE));
        assert!(error.to_string().contains("spec-loop auth login"));
    }

    #[derive(Clone, Debug)]
    struct AuthorizationCapture {
        state: String,
        nonce: String,
        client_id: String,
        code_challenge: String,
        host_id: String,
    }

    enum FakeTokenResponse {
        Login {
            scopes: String,
            nonce_override: Option<String>,
            issuer_override: Option<String>,
        },
        Refresh {
            scopes: String,
        },
        Error {
            body_secret: String,
        },
    }

    struct FakeLoginRun {
        result: Result<()>,
        temporary: TempDir,
        server: thread::JoinHandle<Vec<String>>,
        authorization: Arc<Mutex<Option<AuthorizationCapture>>>,
        saved_registration: Arc<Mutex<Option<Registration>>>,
    }

    fn run_fake_login(
        token_response: FakeTokenResponse,
        expected_state: bool,
        cancel_at_callback: bool,
    ) -> FakeLoginRun {
        let temporary = TempDir::new().expect("create fake OAuth storage");
        run_fake_login_in(
            temporary,
            token_response,
            expected_state,
            cancel_at_callback,
        )
    }

    fn run_fake_login_in(
        temporary: TempDir,
        token_response: FakeTokenResponse,
        expected_state: bool,
        cancel_at_callback: bool,
    ) -> FakeLoginRun {
        let path = temporary.path().join("spec-loop/auth.json");
        let authorization = Arc::new(Mutex::new(None));
        let expected_requests = usize::from(expected_state && !cancel_at_callback) + 2;
        let (issuer, server) = start_fake_issuer(
            token_response,
            expected_requests,
            Arc::clone(&authorization),
        );
        let expected_issuer = issuer.clone();
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("build fake OAuth client");
        let browser_capture = Arc::clone(&authorization);
        let callback_capture = Arc::clone(&authorization);
        let saved_registration = Arc::new(Mutex::new(None));
        let save_capture = Arc::clone(&saved_registration);
        let cancelled = Arc::new(AtomicBool::new(false));
        let result = crate::runtime::block_on(login_with_services(
            path,
            None,
            false,
            cancelled,
            LoginServices {
                issuer_url: &issuer,
                http: &http,
                open_browser: move |url: &str| {
                    capture_authorization_url(url, &browser_capture);
                    Ok::<(), ()>(())
                },
                receive_callback:
                    move |listener: TcpListener, port: u16, cancelled: &Arc<AtomicBool>| {
                        fake_callback(
                            listener,
                            port,
                            cancelled,
                            &callback_capture,
                            expected_state,
                            cancel_at_callback,
                        )
                    },
                save_registration:
                    move |registry: &mut Registry,
                          _path: &Path,
                          issuer: String,
                          subject: String,
                          client_id: String,
                          credential: Credential,
                          selected_id: Option<&str>| {
                        assert_eq!(issuer, expected_issuer);
                        save_test_registration(FakeRegistrationSave {
                            registry,
                            issuer,
                            subject,
                            client_id,
                            credential,
                            selected_id,
                            capture: &save_capture,
                        })
                    },
            },
        ))
        .expect("construct OAuth runtime");
        FakeLoginRun {
            result,
            temporary,
            server,
            authorization,
            saved_registration,
        }
    }

    fn assert_host_id_persisted(
        temporary: &TempDir,
        capture: &Mutex<Option<AuthorizationCapture>>,
    ) {
        let expected_host_id = capture
            .lock()
            .expect("lock authorization capture")
            .as_ref()
            .expect("authorization URL was presented")
            .host_id
            .clone();
        let registry = Registry::load(&temporary.path().join("spec-loop/auth.json"))
            .expect("load pre-authorization registry");
        assert_eq!(registry.host_id, expected_host_id);
        assert!(registry.registrations.is_empty());
        assert!(registry.active_registration_id.is_none());
    }

    fn capture_authorization_url(
        authorization_url: &str,
        capture: &Mutex<Option<AuthorizationCapture>>,
    ) {
        let url = Url::parse(authorization_url).expect("parse authorization URL");
        let query = url
            .query_pairs()
            .collect::<std::collections::BTreeMap<_, _>>();
        let captured = AuthorizationCapture {
            state: query.get("state").expect("authorization state").to_string(),
            nonce: query.get("nonce").expect("authorization nonce").to_string(),
            client_id: query
                .get("client_id")
                .expect("authorization client ID")
                .to_string(),
            code_challenge: query
                .get("code_challenge")
                .expect("PKCE challenge")
                .to_string(),
            host_id: query
                .get("ext_agent_host_id")
                .expect("stable SIWC host ID")
                .to_string(),
        };
        assert_eq!(
            query
                .get("code_challenge_method")
                .map(std::convert::AsRef::as_ref),
            Some("S256")
        );
        assert_eq!(
            query.get("resource").map(std::convert::AsRef::as_ref),
            Some("https://api.openai.com/v1")
        );
        assert!(!authorization_url.contains("fixture-access-secret"));
        *capture.lock().expect("lock authorization capture") = Some(captured);
    }

    fn fake_callback(
        _listener: TcpListener,
        port: u16,
        cancelled: &Arc<AtomicBool>,
        capture: &Mutex<Option<AuthorizationCapture>>,
        expected_state: bool,
        cancel_at_callback: bool,
    ) -> Result<Callback> {
        assert_ne!(port, 0);
        if cancel_at_callback {
            cancelled.store(true, Ordering::Relaxed);
            return Err(SpecLoopError::Interrupted);
        }
        let captured = capture
            .lock()
            .expect("lock callback capture")
            .clone()
            .expect("browser ran before callback");
        Ok(Callback {
            state: if expected_state {
                captured.state
            } else {
                "incorrect-state".to_owned()
            },
            code: Some("fixture-authorization-code".to_owned()),
            client_id: Some("oaiapp_test_a".to_owned()),
            error: None,
        })
    }

    struct FakeRegistrationSave<'a> {
        registry: &'a mut Registry,
        issuer: String,
        subject: String,
        client_id: String,
        credential: Credential,
        selected_id: Option<&'a str>,
        capture: &'a Mutex<Option<Registration>>,
    }

    fn save_test_registration(save: FakeRegistrationSave<'_>) -> Result<()> {
        let FakeRegistrationSave {
            registry,
            issuer,
            subject,
            client_id,
            credential,
            selected_id,
            capture,
        } = save;
        validate_credential(&credential)?;
        let id = registration_id(&issuer, &subject, &client_id)?;
        if selected_id.is_some_and(|selected| selected != id) {
            return Err(SpecLoopError::Config(
                "test registration identity mismatch".to_owned(),
            ));
        }
        let registration = Registration {
            id: id.clone(),
            issuer,
            subject,
            client_id,
            credential_storage: CredentialStorage::File,
            fallback_credential: Some(credential),
        };
        registry.registrations.push(registration.clone());
        registry.active_registration_id = Some(id);
        *capture.lock().expect("lock saved registration") = Some(registration);
        Ok(())
    }

    fn start_fake_issuer(
        token_response: FakeTokenResponse,
        expected_requests: usize,
        capture: Arc<Mutex<Option<AuthorizationCapture>>>,
    ) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake OIDC issuer");
        let address = listener.local_addr().expect("fake OIDC address");
        let issuer = format!("http://{address}");
        let signing_key = test_signing_key();
        let jwks = serde_json::to_string(&CoreJsonWebKeySet::new(vec![
            signing_key.as_verification_key(),
        ]))
        .expect("serialize fake issuer JWKS");
        let discovery = serde_json::to_string(&json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": format!("{issuer}/jwks"),
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "token_endpoint_auth_methods_supported": ["none"]
        }))
        .expect("serialize fake OIDC discovery");
        let server_issuer = issuer.clone();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..expected_requests {
                let (mut stream, _) = listener.accept().expect("accept fake OIDC request");
                let request = read_oauth_http_request(&mut stream);
                let target = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .expect("OAuth request target");
                let (status, body) = match target {
                    "/.well-known/openid-configuration" => ("200 OK", discovery.clone()),
                    "/jwks" => ("200 OK", jwks.clone()),
                    "/token" => match &token_response {
                        FakeTokenResponse::Login {
                            scopes,
                            nonce_override,
                            issuer_override,
                        } => {
                            let captured = capture
                                .lock()
                                .expect("lock token-request authorization capture")
                                .clone()
                                .expect("authorization URL capture before token exchange");
                            let issuer = issuer_override.as_deref().unwrap_or(&server_issuer);
                            let nonce = nonce_override.as_deref().unwrap_or(&captured.nonce);
                            let (_, token_body) =
                                request.split_once("\r\n\r\n").expect("token request body");
                            let token_form = url::form_urlencoded::parse(token_body.as_bytes())
                                .into_owned()
                                .collect::<std::collections::BTreeMap<_, _>>();
                            let token_client_id = token_form
                                .get("client_id")
                                .expect("client ID in token exchange");
                            let id_token = signed_test_id_token(
                                issuer,
                                nonce,
                                token_client_id,
                                "test-subject-a",
                                &signing_key,
                            );
                            (
                                "200 OK",
                                json!({
                                    "access_token": "fixture-access-secret",
                                    "refresh_token": "fixture-refresh-secret",
                                    "token_type": "Bearer",
                                    "expires_in": 3600,
                                    "scope": scopes,
                                    "id_token": id_token
                                })
                                .to_string(),
                            )
                        }
                        FakeTokenResponse::Refresh { scopes } => (
                            "200 OK",
                            json!({
                                "access_token": "fixture-refreshed-access",
                                "refresh_token": "fixture-refreshed-refresh",
                                "token_type": "Bearer",
                                "expires_in": 3600,
                                "scope": scopes
                            })
                            .to_string(),
                        ),
                        FakeTokenResponse::Error { body_secret } => (
                            "400 Bad Request",
                            json!({"error": "invalid_grant", "detail": body_secret}).to_string(),
                        ),
                    },
                    _ => panic!("unexpected fake OIDC request target: {target}"),
                };
                write_oauth_response(&mut stream, status, &body);
                requests.push(request);
            }
            requests
        });
        (issuer, server)
    }

    fn test_signing_key() -> CoreRsaPrivateSigningKey {
        CoreRsaPrivateSigningKey::from_pem(
            include_str!("../tests/fixtures/oidc-test-rsa-key.pem"),
            Some(JsonWebKeyId::new("spec-loop-test-key".to_owned())),
        )
        .expect("parse fixed test-only OIDC signing key")
    }

    fn signed_test_id_token(
        issuer: &str,
        nonce: &str,
        client_id: &str,
        subject: &str,
        signing_key: &CoreRsaPrivateSigningKey,
    ) -> String {
        let now = Utc::now();
        let claims = CoreIdTokenClaims::new(
            IssuerUrl::new(issuer.to_owned()).expect("create ID-token issuer"),
            vec![Audience::new(client_id.to_owned())],
            now + ChronoDuration::minutes(5),
            now,
            StandardClaims::new(SubjectIdentifier::new(subject.to_owned())),
            EmptyAdditionalClaims {},
        )
        .set_nonce(Some(Nonce::new(nonce.to_owned())));
        CoreIdToken::new(
            claims,
            signing_key,
            CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
            Some(&AccessToken::new("fixture-access-secret".to_owned())),
            None,
        )
        .expect("sign fake OIDC ID token")
        .to_string()
    }

    fn read_oauth_http_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 2048];
        loop {
            let read = stream.read(&mut buffer).expect("read fake OAuth request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("OAuth content length"))
                })
                .unwrap_or_default();
            if request.len() >= header_end + 4 + content_length {
                break;
            }
        }
        String::from_utf8(request).expect("fake OAuth request is UTF-8")
    }

    fn write_oauth_response(stream: &mut TcpStream, status: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("write fake OAuth response");
    }

    #[test]
    fn login_exchanges_code_and_validates_pkce_state_and_signed_identity_offline() {
        let FakeLoginRun {
            result,
            server,
            authorization: capture,
            saved_registration: saved,
            ..
        } = run_fake_login(
            FakeTokenResponse::Login {
                scopes: "openid offline_access chatgpt.tokens.use.direct".to_owned(),
                nonce_override: None,
                issuer_override: None,
            },
            true,
            false,
        );
        result.expect("complete fake OAuth login");
        let requests = server.join().expect("join fake OIDC server");
        assert_eq!(requests.len(), 3);
        assert!(requests[0].starts_with("GET /.well-known/openid-configuration "));
        assert!(requests[1].starts_with("GET /jwks "));
        assert!(requests[2].starts_with("POST /token "));

        let captured = capture
            .lock()
            .expect("lock authorization capture")
            .clone()
            .expect("capture authorization parameters");
        assert_ne!(captured.state, "");
        assert_eq!(captured.client_id, "dynamic_agent_client");
        let (_, token_body) = requests[2]
            .split_once("\r\n\r\n")
            .expect("token request body");
        let token_form = url::form_urlencoded::parse(token_body.as_bytes())
            .into_owned()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            token_form.get("code"),
            Some(&"fixture-authorization-code".to_owned())
        );
        assert_eq!(
            token_form.get("client_id"),
            Some(&"oaiapp_test_a".to_owned())
        );
        assert_eq!(
            token_form.get("resource"),
            Some(&"https://api.openai.com/v1".to_owned())
        );
        let verifier = PkceCodeVerifier::new(
            token_form
                .get("code_verifier")
                .expect("PKCE verifier in token exchange")
                .clone(),
        );
        assert_eq!(
            PkceCodeChallenge::from_code_verifier_sha256(&verifier).as_str(),
            captured.code_challenge
        );

        let registration = saved
            .lock()
            .expect("lock saved registration")
            .clone()
            .expect("validated identity was handed to credential storage");
        assert!(registration.issuer.starts_with("http://127.0.0.1:"));
        assert_eq!(registration.subject, "test-subject-a");
        assert_eq!(registration.client_id, "oaiapp_test_a");
        assert!(
            registration
                .fallback_credential
                .as_ref()
                .is_some_and(|credential| credential
                    .scopes
                    .iter()
                    .any(|scope| scope == DIRECT_SCOPE))
        );
    }

    #[test]
    fn login_rejects_missing_permission_wrong_state_and_invalid_nonce_without_saving_credentials() {
        let FakeLoginRun {
            result: missing_permission,
            temporary,
            server,
            authorization: capture,
            saved_registration: saved,
            ..
        } = run_fake_login(
            FakeTokenResponse::Login {
                scopes: "openid offline_access".to_owned(),
                nonce_override: None,
                issuer_override: None,
            },
            true,
            false,
        );
        let error = missing_permission.expect_err("reject token without direct-use permission");
        assert!(error.to_string().contains(DIRECT_SCOPE));
        assert!(!error.to_string().contains("fixture-access-secret"));
        assert_eq!(server.join().expect("join fake issuer").len(), 3);
        assert!(saved.lock().expect("lock save capture").is_none());
        assert_host_id_persisted(&temporary, &capture);

        let FakeLoginRun {
            result: wrong_state,
            temporary,
            server,
            authorization: capture,
            saved_registration: saved,
            ..
        } = run_fake_login(
            FakeTokenResponse::Login {
                scopes: "openid offline_access chatgpt.tokens.use.direct".to_owned(),
                nonce_override: None,
                issuer_override: None,
            },
            false,
            false,
        );
        assert!(
            wrong_state
                .expect_err("reject callback with mismatched state")
                .to_string()
                .contains("state validation")
        );
        assert_eq!(server.join().expect("join fake issuer").len(), 2);
        assert!(saved.lock().expect("lock save capture").is_none());
        assert_host_id_persisted(&temporary, &capture);

        let FakeLoginRun {
            result: wrong_nonce,
            temporary,
            server,
            authorization: capture,
            saved_registration: saved,
            ..
        } = run_fake_login(
            FakeTokenResponse::Login {
                scopes: "openid offline_access chatgpt.tokens.use.direct".to_owned(),
                nonce_override: Some("wrong-nonce".to_owned()),
                issuer_override: None,
            },
            true,
            false,
        );
        assert!(
            wrong_nonce
                .expect_err("reject ID token with mismatched nonce")
                .to_string()
                .contains("ID token validation failed")
        );
        assert_eq!(server.join().expect("join fake issuer").len(), 3);
        assert!(saved.lock().expect("lock save capture").is_none());
        assert_host_id_persisted(&temporary, &capture);

        let FakeLoginRun {
            result: wrong_issuer,
            temporary,
            server,
            authorization: capture,
            saved_registration: saved,
            ..
        } = run_fake_login(
            FakeTokenResponse::Login {
                scopes: "openid offline_access chatgpt.tokens.use.direct".to_owned(),
                nonce_override: None,
                issuer_override: Some("http://issuer-mismatch.invalid".to_owned()),
            },
            true,
            false,
        );
        assert!(
            wrong_issuer
                .expect_err("reject ID token from another issuer")
                .to_string()
                .contains("ID token validation failed")
        );
        assert_eq!(server.join().expect("join fake issuer").len(), 3);
        assert!(saved.lock().expect("lock save capture").is_none());
        assert_host_id_persisted(&temporary, &capture);
    }

    #[test]
    fn failed_first_login_retry_reuses_persisted_host_id() {
        let first_attempt = run_fake_login(
            FakeTokenResponse::Login {
                scopes: "openid offline_access chatgpt.tokens.use.direct".to_owned(),
                nonce_override: None,
                issuer_override: None,
            },
            false,
            false,
        );
        let FakeLoginRun {
            result,
            temporary,
            server,
            authorization: first_authorization,
            ..
        } = first_attempt;
        assert!(
            result
                .expect_err("reject first login with invalid callback state")
                .to_string()
                .contains("state validation")
        );
        assert_eq!(server.join().expect("join failed login server").len(), 2);
        assert_host_id_persisted(&temporary, &first_authorization);
        let first_host_id = first_authorization
            .lock()
            .expect("lock first authorization capture")
            .as_ref()
            .expect("first authorization parameters")
            .host_id
            .clone();

        let retry = run_fake_login_in(
            temporary,
            FakeTokenResponse::Login {
                scopes: "openid offline_access chatgpt.tokens.use.direct".to_owned(),
                nonce_override: None,
                issuer_override: None,
            },
            true,
            false,
        );
        let FakeLoginRun {
            result,
            server,
            authorization: retry_authorization,
            ..
        } = retry;
        result.expect("retry fake OAuth login");
        assert_eq!(server.join().expect("join retry login server").len(), 3);
        let retry_host_id = retry_authorization
            .lock()
            .expect("lock retry authorization capture")
            .as_ref()
            .expect("retry authorization parameters")
            .host_id
            .clone();
        assert_eq!(retry_host_id, first_host_id);
    }

    #[test]
    fn login_cancellation_and_token_errors_do_not_leak_or_persist_secrets() {
        let FakeLoginRun {
            result: cancelled,
            temporary,
            server,
            authorization: capture,
            saved_registration: saved,
            ..
        } = run_fake_login(
            FakeTokenResponse::Error {
                body_secret: "private-token-error-body".to_owned(),
            },
            true,
            true,
        );
        assert!(matches!(cancelled, Err(SpecLoopError::Interrupted)));
        assert_eq!(server.join().expect("join cancelled fake issuer").len(), 2);
        assert!(saved.lock().expect("lock save capture").is_none());
        assert_host_id_persisted(&temporary, &capture);

        let FakeLoginRun {
            result: token_error,
            temporary,
            server,
            authorization: capture,
            saved_registration: saved,
            ..
        } = run_fake_login(
            FakeTokenResponse::Error {
                body_secret: "private-token-error-body".to_owned(),
            },
            true,
            false,
        );
        let error = token_error.expect_err("reject fake token endpoint error");
        assert!(error.to_string().contains("OpenAI token exchange failed"));
        assert!(!error.to_string().contains("private-token-error-body"));
        assert_eq!(server.join().expect("join failed fake issuer").len(), 3);
        assert!(saved.lock().expect("lock save capture").is_none());
        assert_host_id_persisted(&temporary, &capture);
    }

    #[test]
    fn refresh_success_and_failure_use_fake_token_endpoint_without_secret_diagnostics() {
        let (issuer, server) = start_fake_issuer(
            FakeTokenResponse::Refresh {
                scopes: "openid offline_access chatgpt.tokens.use.direct".to_owned(),
            },
            3,
            Arc::new(Mutex::new(None)),
        );
        let registration = Registration {
            id: "registration-id".to_owned(),
            issuer,
            subject: "test-subject-a".to_owned(),
            client_id: "oaiapp_test_a".to_owned(),
            credential_storage: CredentialStorage::LoggedOut,
            fallback_credential: None,
        };
        let current = Credential {
            access_token: "expired-access".to_owned(),
            refresh_token: Some("fixture-refresh-secret".to_owned()),
            id_token: "fixture-id-token".to_owned(),
            scopes: vec![DIRECT_SCOPE.to_owned()],
            expires_at_unix: Some(1),
        };
        let cancelled = AtomicBool::new(false);
        let refreshed =
            crate::runtime::block_on(refresh_credential(&registration, &current, &cancelled))
                .expect("construct refresh runtime")
                .expect("refresh fake token");
        assert_eq!(refreshed.access_token, "fixture-refreshed-access");
        assert_eq!(
            refreshed.refresh_token.as_deref(),
            Some("fixture-refreshed-refresh")
        );
        assert_eq!(server.join().expect("join fake refresh issuer").len(), 3);

        let (issuer, server) = start_fake_issuer(
            FakeTokenResponse::Error {
                body_secret: "private-refresh-error-body".to_owned(),
            },
            3,
            Arc::new(Mutex::new(None)),
        );
        let registration = Registration {
            issuer,
            ..registration
        };
        let error =
            crate::runtime::block_on(refresh_credential(&registration, &current, &cancelled))
                .expect("construct refresh runtime")
                .expect_err("reject fake refresh failure");
        assert!(error.to_string().contains("expired or were revoked"));
        assert!(!error.to_string().contains("private-refresh-error-body"));
        assert_eq!(server.join().expect("join failed refresh issuer").len(), 3);
    }

    fn authorization_test_client() -> ConfiguredCoreClient {
        use openidconnect::core::{
            CoreClient, CoreJwsSigningAlgorithm, CoreProviderMetadata, CoreResponseType,
            CoreSubjectIdentifierType,
        };
        use openidconnect::{
            AuthUrl, ClientId, EmptyAdditionalProviderMetadata, IssuerUrl, JsonWebKeySetUrl,
            RedirectUrl, ResponseTypes, TokenUrl,
        };

        let provider = CoreProviderMetadata::new(
            IssuerUrl::new("https://auth.openai.com".to_owned()).expect("issuer URL"),
            AuthUrl::new("https://auth.openai.com/authorize".to_owned())
                .expect("authorization URL"),
            JsonWebKeySetUrl::new("https://auth.openai.com/jwks".to_owned()).expect("JWKS URL"),
            vec![ResponseTypes::new(vec![CoreResponseType::Code])],
            vec![CoreSubjectIdentifierType::Public],
            vec![CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256],
            EmptyAdditionalProviderMetadata {},
        )
        .set_token_endpoint(Some(
            TokenUrl::new("https://auth.openai.com/token".to_owned()).expect("token URL"),
        ));
        CoreClient::from_provider_metadata(
            provider,
            ClientId::new("issued-client-id".to_owned()),
            None,
        )
        .set_redirect_uri(
            RedirectUrl::new(format!("http://127.0.0.1:43210{CALLBACK_PATH}"))
                .expect("redirect URL"),
        )
    }

    fn authorization_parameter<'a>(
        parameters: &'a [(std::borrow::Cow<'a, str>, std::borrow::Cow<'a, str>)],
        name: &str,
    ) -> Option<&'a str> {
        parameters
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, value)| value.as_ref())
    }

    #[test]
    fn new_registration_authorization_requests_documented_scopes_and_hints() {
        let client = authorization_test_client();
        let host_id = "urn:uuid:00000000-0000-4000-8000-000000000001";
        let authorization = authorization_details(&client, host_id, AuthorizationRegistration::New);
        let parameters = authorization.url.query_pairs().collect::<Vec<_>>();

        assert_eq!(
            authorization_parameter(&parameters, "client_id"),
            Some("issued-client-id")
        );
        assert_eq!(
            authorization_parameter(&parameters, "state"),
            Some(authorization.state.secret().as_str())
        );
        assert!(
            authorization_parameter(&parameters, "nonce").is_some_and(|value| !value.is_empty())
        );
        assert_eq!(
            authorization_parameter(&parameters, "code_challenge_method"),
            Some("S256")
        );
        assert!(
            authorization_parameter(&parameters, "code_challenge")
                .is_some_and(|challenge| challenge.len() == 43)
        );
        assert_eq!(
            authorization_parameter(&parameters, "resource"),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(
            authorization_parameter(&parameters, "agent_name_hint"),
            Some("Spec Loop")
        );
        assert_eq!(authorization_parameter(&parameters, "id_token_hint"), None);
        assert_eq!(
            authorization_parameter(&parameters, "ext_agent_host_id"),
            Some(host_id)
        );
        let scope =
            authorization_parameter(&parameters, "scope").expect("requested authorization scopes");
        for required_scope in [
            "openid",
            "profile",
            "email",
            "offline_access",
            "resource.invoke",
            DIRECT_SCOPE,
        ] {
            assert!(
                scope.contains(required_scope),
                "missing scope {required_scope}"
            );
        }
    }

    #[test]
    fn returning_registration_uses_id_token_hint_without_name_hint() {
        let client = authorization_test_client();
        let host_id = "urn:uuid:00000000-0000-4000-8000-000000000001";
        let authorization = authorization_details(
            &client,
            host_id,
            AuthorizationRegistration::Returning {
                id_token_hint: Some("stored-id-token-secret"),
            },
        );
        let parameters = authorization.url.query_pairs().collect::<Vec<_>>();
        assert_eq!(
            authorization_parameter(&parameters, "id_token_hint"),
            Some("stored-id-token-secret")
        );
        assert_eq!(
            authorization_parameter(&parameters, "agent_name_hint"),
            None
        );

        let without_id_token = authorization_details(
            &client,
            host_id,
            AuthorizationRegistration::Returning {
                id_token_hint: None,
            },
        );
        let without_hint_parameters = without_id_token.url.query_pairs().collect::<Vec<_>>();
        assert_eq!(
            authorization_parameter(&without_hint_parameters, "id_token_hint"),
            None
        );
        assert_eq!(
            authorization_parameter(&without_hint_parameters, "agent_name_hint"),
            None
        );
    }

    #[test]
    fn browser_opener_never_receives_id_token_hint() {
        let secret = "stored-id-token-secret";
        let url = url::Url::parse(&format!(
            "https://auth.openai.com/authorize?state=fixture&id_token_hint={secret}&client_id=fixture"
        ))
        .expect("parse fixture authorization URL");
        let mut messages = Vec::new();
        let mut opened_url = None;
        present_authorization_url(
            &url,
            |value| {
                opened_url = Some(value.to_owned());
                assert!(!value.contains(secret));
                assert!(value.contains("state=fixture"));
                assert!(value.contains("client_id=fixture"));
                Ok::<(), &str>(())
            },
            |message| match message {
                BrowserMessage::AuthorizationUrl(value) => {
                    messages.push(format!("authorization URL: {value}"));
                }
                BrowserMessage::ManualOpenFallback => {
                    messages.push("manual-open fallback".to_owned());
                }
            },
        );
        let printed = messages.join("\n");
        assert!(!printed.contains(secret));
        assert!(printed.contains("state=fixture"));
        assert!(printed.contains("client_id=fixture"));
        assert_eq!(opened_url, Some(manual_authorization_url(&url).to_string()));
        assert_eq!(messages.len(), 1);

        let mut fallback_messages = Vec::new();
        present_authorization_url(
            &url,
            |_| Err("browser unavailable"),
            |message| match message {
                BrowserMessage::AuthorizationUrl(value) => {
                    fallback_messages.push(format!("authorization URL: {value}"));
                }
                BrowserMessage::ManualOpenFallback => {
                    fallback_messages.push("manual-open fallback".to_owned());
                }
            },
        );
        let fallback_output = fallback_messages.join("\n");
        assert!(!fallback_output.contains(secret));
        assert!(fallback_output.contains("manual-open fallback"));
    }

    #[test]
    fn callback_validates_state_and_preserves_selected_client_registration() {
        let selected = Registration {
            id: "registration-id".to_owned(),
            issuer: "https://auth.openai.com".to_owned(),
            subject: "verified-subject".to_owned(),
            client_id: "issued-client-id".to_owned(),
            credential_storage: CredentialStorage::LoggedOut,
            fallback_credential: None,
        };
        let callback = Callback {
            state: "expected-state".to_owned(),
            code: Some("authorization-code".to_owned()),
            client_id: None,
            error: None,
        };
        let (_, reused_id) = validate_callback(callback, "expected-state", Some(&selected))
            .expect("accept returning registration callback without client ID");
        assert_eq!(reused_id, "issued-client-id");

        let mismatched = Callback {
            state: "expected-state".to_owned(),
            code: Some("authorization-code".to_owned()),
            client_id: Some("other-client-id".to_owned()),
            error: None,
        };
        assert!(validate_callback(mismatched, "expected-state", Some(&selected)).is_err());

        let wrong_state = Callback {
            state: "unexpected-state".to_owned(),
            code: Some("authorization-code".to_owned()),
            client_id: Some("issued-client-id".to_owned()),
            error: None,
        };
        assert!(validate_callback(wrong_state, "expected-state", Some(&selected)).is_err());
    }

    #[test]
    fn registration_ids_bind_issuer_subject_and_client_id() {
        let original =
            registration_id("issuer", "subject-a", "client-a").expect("encode registration ID");
        assert_eq!(original.len(), 64);
        assert_ne!(
            original,
            registration_id("issuer", "subject-b", "client-a").unwrap()
        );
        assert_ne!(
            original,
            registration_id("issuer", "subject-a", "client-b").unwrap()
        );
    }

    #[test]
    fn registry_normalizes_legacy_host_id_to_uuid_uri() {
        let temporary = TempDir::new().expect("create auth temp directory");
        let path = temporary.path().join("auth.json");
        let mut registry = Registry::empty();
        let expected_host_id = registry.host_id.clone();
        registry.host_id = expected_host_id
            .strip_prefix("urn:uuid:")
            .expect("new host ID uses UUID URI")
            .to_owned();
        let bytes = serde_json::to_vec(&registry).expect("serialize legacy auth registry");
        std::fs::write(&path, bytes).expect("write legacy auth registry");

        let loaded = Registry::load(&path).expect("load legacy auth registry");
        assert_eq!(loaded.host_id, expected_host_id);
    }

    #[test]
    fn registry_round_trip_preserves_owner_file_fallback_credential() {
        let temporary = TempDir::new().expect("create auth temp directory");
        let path = temporary.path().join("spec-loop/auth.json");
        let mut registry = Registry::empty();
        let registration_id = registration_id(
            "https://auth.openai.com",
            "verified-subject",
            "issued-client-id",
        )
        .expect("create registration ID");
        registry.registrations.push(Registration {
            id: registration_id.clone(),
            issuer: "https://auth.openai.com".to_owned(),
            subject: "verified-subject".to_owned(),
            client_id: "issued-client-id".to_owned(),
            credential_storage: CredentialStorage::File,
            fallback_credential: Some(Credential {
                access_token: "access-secret".to_owned(),
                refresh_token: Some("refresh-secret".to_owned()),
                id_token: "id-secret".to_owned(),
                scopes: vec!["chatgpt.tokens.use.direct".to_owned()],
                expires_at_unix: Some(1_900_000_000),
            }),
        });
        registry.active_registration_id = Some(registration_id);
        registry.save(&path).expect("save auth registry");
        let loaded = Registry::load(&path).expect("load auth registry");
        assert_eq!(loaded.host_id, registry.host_id);
        assert_eq!(
            loaded
                .registrations
                .first()
                .expect("registration")
                .client_id,
            "issued-client-id"
        );
        let serialized = std::fs::read_to_string(path).expect("read auth registry");
        assert!(serialized.contains("access-secret"));
    }

    fn test_credential(access_token: &str) -> Credential {
        Credential {
            access_token: access_token.to_owned(),
            refresh_token: Some("refresh-token".to_owned()),
            id_token: "identity-token".to_owned(),
            scopes: vec![DIRECT_SCOPE.to_owned()],
            expires_at_unix: Some(1_900_000_000),
        }
    }

    fn test_registration_update(
        issuer: &str,
        subject: &str,
        client_id: &str,
        credential: Credential,
        selected_id: Option<&str>,
    ) -> RegistrationUpdate {
        RegistrationUpdate {
            registration: Registration {
                id: registration_id(issuer, subject, client_id)
                    .expect("create test registration ID"),
                issuer: issuer.to_owned(),
                subject: subject.to_owned(),
                client_id: client_id.to_owned(),
                credential_storage: CredentialStorage::Keyring,
                fallback_credential: None,
            },
            credential,
            selected_id: selected_id.map(str::to_owned),
        }
    }

    #[test]
    fn only_explicitly_unavailable_keyring_errors_allow_file_fallback() {
        assert!(credential_store_unavailable(
            &keyring::Error::NoDefaultStore
        ));
        assert!(credential_store_unavailable(
            &keyring::Error::NotSupportedByStore("unsupported platform".to_owned())
        ));
        assert!(!credential_store_unavailable(
            &keyring::Error::NoStorageAccess(Box::new(std::io::Error::other("store locked")),)
        ));
        assert!(!credential_store_unavailable(
            &keyring::Error::PlatformFailure(Box::new(std::io::Error::other("write failed")),)
        ));
    }

    #[test]
    fn unavailable_keyring_uses_file_but_available_store_write_failure_propagates() {
        let temporary = TempDir::new().expect("create registration fixture");
        let path = temporary.path().join("auth.json");
        let mut registry = Registry::empty();
        let input = ("https://auth.openai.com", "subject-a", "client-a");
        persist_registration_with(
            &mut registry,
            &path,
            test_registration_update(
                input.0,
                input.1,
                input.2,
                test_credential("file-token"),
                None,
            ),
            RegistrationPersistence {
                save_registry: Registry::save,
                write_credential: |_: &str, _: &Credential| Err(CredentialWriteError::Unavailable),
                read_credential: |_: &Registration| Ok(None),
                restore_credential: |_: &str, _: Option<&Credential>| Ok(()),
            },
        )
        .expect("unavailable OS store should use file fallback");
        let registration = registry
            .registrations
            .first()
            .expect("file-backed registration");
        assert!(matches!(
            registration.credential_storage,
            CredentialStorage::File
        ));
        assert_eq!(
            registration
                .fallback_credential
                .as_ref()
                .map(|credential| credential.access_token.as_str()),
            Some("file-token")
        );

        let failure_path = temporary.path().join("failed/auth.json");
        let mut failed_registry = Registry::empty();
        let error = persist_registration_with(
            &mut failed_registry,
            &failure_path,
            test_registration_update(
                input.0,
                input.1,
                input.2,
                test_credential("must-not-fallback"),
                None,
            ),
            RegistrationPersistence {
                save_registry: Registry::save,
                write_credential: |_: &str, _: &Credential| {
                    Err(CredentialWriteError::Failed(SpecLoopError::Workflow(
                        "keyring write failed".to_owned(),
                    )))
                },
                read_credential: |_: &Registration| Ok(None),
                restore_credential: |_: &str, _: Option<&Credential>| Ok(()),
            },
        )
        .expect_err("available-store write failure must propagate");
        assert!(error.to_string().contains("keyring write failed"));
        let persisted = Registry::load(&failure_path).expect("read rolled-back registry");
        assert!(persisted.registrations.is_empty());
        assert!(persisted.active_registration_id.is_none());
    }

    #[test]
    fn registry_activation_failure_removes_new_keyring_credential_and_metadata() {
        let temporary = TempDir::new().expect("create registration fixture");
        let path = temporary.path().join("auth.json");
        let mut registry = Registry::empty();
        let keyring = Arc::new(Mutex::new(HashMap::<String, Credential>::new()));
        let saves = Cell::new(0_usize);
        let save_result = persist_registration_with(
            &mut registry,
            &path,
            test_registration_update(
                "https://auth.openai.com",
                "subject-new",
                "client-new",
                test_credential("new-token"),
                None,
            ),
            RegistrationPersistence {
                save_registry: |candidate: &Registry, path: &Path| {
                    let save_number = saves.get().checked_add(1).expect("save counter");
                    saves.set(save_number);
                    if save_number == 2 {
                        Err(SpecLoopError::Workflow(
                            "injected active-registry write failure".to_owned(),
                        ))
                    } else {
                        candidate.save(path)
                    }
                },
                write_credential: {
                    let keyring = Arc::clone(&keyring);
                    move |id: &str, credential: &Credential| {
                        keyring
                            .lock()
                            .expect("lock fake keyring")
                            .insert(id.to_owned(), credential.clone());
                        Ok(())
                    }
                },
                read_credential: |_: &Registration| Ok(None),
                restore_credential: {
                    let keyring = Arc::clone(&keyring);
                    move |id: &str, previous: Option<&Credential>| {
                        {
                            let mut entries = keyring.lock().expect("lock fake keyring");
                            if let Some(credential) = previous {
                                entries.insert(id.to_owned(), credential.clone());
                            } else {
                                entries.remove(id);
                            }
                        }
                        Ok(())
                    }
                },
            },
        );
        assert!(save_result.is_err());
        assert!(
            Registry::load(&path)
                .expect("load restored registry")
                .registrations
                .is_empty()
        );
        assert!(keyring.lock().expect("lock fake keyring").is_empty());
        assert!(registry.registrations.is_empty());
        assert!(registry.active_registration_id.is_none());
    }

    #[test]
    fn registry_activation_failure_restores_existing_keyring_credential() {
        let temporary = TempDir::new().expect("create registration fixture");
        let path = temporary.path().join("auth.json");
        let mut registry = Registry::empty();
        let id = registration_id("https://auth.openai.com", "subject-old", "client-old")
            .expect("create existing registration ID");
        registry.registrations.push(Registration {
            id: id.clone(),
            issuer: "https://auth.openai.com".to_owned(),
            subject: "subject-old".to_owned(),
            client_id: "client-old".to_owned(),
            credential_storage: CredentialStorage::Keyring,
            fallback_credential: None,
        });
        registry.active_registration_id = Some(id.clone());
        registry.save(&path).expect("save existing registry");
        let keyring = Arc::new(Mutex::new(HashMap::from([(
            id.clone(),
            test_credential("old-token"),
        )])));
        let saves = Cell::new(0_usize);
        let save_result = persist_registration_with(
            &mut registry,
            &path,
            test_registration_update(
                "https://auth.openai.com",
                "subject-old",
                "client-old",
                test_credential("new-token"),
                Some(&id),
            ),
            RegistrationPersistence {
                save_registry: |candidate: &Registry, path: &Path| {
                    let save_number = saves.get().checked_add(1).expect("save counter");
                    saves.set(save_number);
                    if save_number == 2 {
                        Err(SpecLoopError::Workflow(
                            "injected active-registry write failure".to_owned(),
                        ))
                    } else {
                        candidate.save(path)
                    }
                },
                write_credential: {
                    let keyring = Arc::clone(&keyring);
                    move |id: &str, credential: &Credential| {
                        keyring
                            .lock()
                            .expect("lock fake keyring")
                            .insert(id.to_owned(), credential.clone());
                        Ok(())
                    }
                },
                read_credential: {
                    let keyring = Arc::clone(&keyring);
                    move |registration: &Registration| {
                        Ok(keyring
                            .lock()
                            .expect("lock fake keyring")
                            .get(&registration.id)
                            .cloned())
                    }
                },
                restore_credential: {
                    let keyring = Arc::clone(&keyring);
                    move |id: &str, previous: Option<&Credential>| {
                        {
                            let mut entries = keyring.lock().expect("lock fake keyring");
                            if let Some(credential) = previous {
                                entries.insert(id.to_owned(), credential.clone());
                            } else {
                                entries.remove(id);
                            }
                        }
                        Ok(())
                    }
                },
            },
        );
        assert!(save_result.is_err());
        assert_eq!(
            keyring
                .lock()
                .expect("lock fake keyring")
                .get(&id)
                .map(|credential| credential.access_token.as_str()),
            Some("old-token")
        );
        let persisted = Registry::load(&path).expect("load existing registry");
        assert_eq!(
            persisted.active_registration_id.as_deref(),
            Some(id.as_str())
        );
        assert_eq!(persisted.registrations.len(), 1);
        assert_eq!(
            registry.active_registration_id.as_deref(),
            Some(id.as_str())
        );
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_relative_xdg_config_home_cannot_write_file_fallback() {
        let project = TempDir::new().expect("create project directory");
        let unsafe_xdg = std::path::Path::new(".");
        let result = registry_path_for_config_dir(unsafe_xdg, Some(project.path()));
        assert!(result.is_err());
        assert!(!project.path().join("spec-loop/auth.json").exists());

        let worktree_path = project.path().join(".config/spec-loop/auth.json");
        assert!(validate_external_registry_path(&worktree_path, Some(project.path())).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn profile_free_auth_storage_does_not_treat_current_directory_as_project_boundary() {
        let home = TempDir::new().expect("create profile-free home");
        let registry = home.path().join(".config/spec-loop/auth.json");
        assert!(!home.path().join(".spec-loop.toml").exists());
        assert_eq!(
            project_boundary(home.path()).expect("detect project boundary"),
            None
        );
        assert_eq!(
            validate_external_registry_path(&registry, None).expect("allow external user config"),
            registry
        );
    }

    #[cfg(unix)]
    #[test]
    fn auth_storage_containment_applies_when_profile_marks_project_root() {
        let project = TempDir::new().expect("create profiled project");
        let profile = project.path().join(".spec-loop.toml");
        std::fs::write(&profile, "").expect("write project profile marker");
        assert_eq!(
            project_boundary(project.path()).expect("detect project boundary"),
            Some(project.path().to_path_buf())
        );
        let nested = project.path().join("source");
        std::fs::create_dir(&nested).expect("create nested project directory");
        assert_eq!(
            project_boundary(&nested).expect("detect ancestor project boundary"),
            Some(project.path().to_path_buf())
        );
        let inside = project.path().join(".config/spec-loop/auth.json");
        assert!(validate_external_registry_path(&inside, Some(project.path())).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn fallback_registry_permissions_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = TempDir::new().expect("create auth temp directory");
        let path = temporary.path().join("nested/auth.json");
        Registry::empty().save(&path).expect("save auth registry");
        let directory_mode = std::fs::metadata(path.parent().expect("parent"))
            .expect("read auth directory metadata")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(path)
            .expect("read auth file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    #[test]
    fn refreshed_token_rotates_credentials_and_requires_direct_permission() {
        let current = Credential {
            access_token: "old-access".to_owned(),
            refresh_token: Some("old-refresh".to_owned()),
            id_token: "identity-token".to_owned(),
            scopes: vec!["chatgpt.tokens.use.direct".to_owned()],
            expires_at_unix: Some(1),
        };
        let refreshed = refreshed_credential(
            &current,
            "new-access".to_owned(),
            Some("new-refresh".to_owned()),
            vec!["chatgpt.tokens.use.direct".to_owned()],
            Some(std::time::Duration::from_secs(3600)),
        )
        .expect("accept authorized refreshed token");
        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(refreshed.id_token, "identity-token");

        let missing_permission =
            refreshed_credential(&current, "new-access".to_owned(), None, Vec::new(), None);
        assert!(missing_permission.is_err());
    }

    #[test]
    fn refresh_starts_before_access_token_expiration() {
        let credential = Credential {
            access_token: "access".to_owned(),
            refresh_token: Some("refresh".to_owned()),
            id_token: "identity".to_owned(),
            scopes: vec!["chatgpt.tokens.use.direct".to_owned()],
            expires_at_unix: Some(1_100),
        };
        assert!(refresh_is_due(&credential, 1_040));
        assert!(!refresh_is_due(&credential, 1_039));
        assert!(refresh_is_due(
            &Credential {
                expires_at_unix: None,
                ..credential
            },
            1_100
        ));
    }

    #[test]
    fn missing_and_unrepresentable_token_expiry_fail_safely() {
        let guidance = "OpenAI token response omitted or exceeded representable expiry; run `spec-loop auth login` again";
        assert!(expiry_timestamp(None, guidance).is_err());
        assert!(expiry_timestamp(Some(Duration::MAX), guidance).is_err());
        assert!(
            validate_credential(&Credential {
                access_token: "access".to_owned(),
                refresh_token: Some("refresh".to_owned()),
                id_token: "identity".to_owned(),
                scopes: vec![DIRECT_SCOPE.to_owned()],
                expires_at_unix: None,
            })
            .is_err()
        );
    }

    #[test]
    fn cancellation_interrupts_pending_oauth_request() {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancellation_flag = std::sync::Arc::clone(&cancelled);
        let cancellation_task = thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(40));
            cancellation_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        let result = crate::runtime::block_on(cancellable(&cancelled, async {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            Ok(())
        }))
        .expect("construct async runtime");
        cancellation_task.join().expect("join cancellation task");
        assert!(matches!(
            result,
            Err(crate::error::SpecLoopError::Interrupted)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn fallback_windows_acl_restricts_auth_directory_and_file() {
        let temporary = TempDir::new().expect("create temporary auth directory");
        let directory = temporary.path().join("private");
        std::fs::create_dir(&directory).expect("create credential directory");
        let file = directory.join("auth.json");
        std::fs::write(&file, b"{}").expect("create credential file");

        super::restrict_directory(&directory).expect("restrict credential directory ACL");
        super::restrict_file(&file).expect("restrict credential file ACL");

        let current_sid = super::current_windows_user_sid().expect("current user SID");
        for path in [&directory, &file] {
            let result = Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "(Get-Acl -LiteralPath $args[0]).Access | ForEach-Object { '{0}|{1}|{2}' -f $_.IdentityReference.Translate([System.Security.Principal.SecurityIdentifier]).Value, $_.IsInherited, $_.FileSystemRights }",
                ])
                .arg(path)
                .output()
                .expect("inspect credential ACL");
            assert!(result.status.success(), "PowerShell ACL inspection failed");
            let output = String::from_utf8(result.stdout).expect("ACL output is UTF-8");
            let entries = output.lines().collect::<Vec<_>>();
            assert_eq!(entries.len(), 2, "unexpected credential ACL: {output}");
            assert!(
                entries.iter().any(|entry| {
                    entry.starts_with(&format!("{current_sid}|False|"))
                        && entry.ends_with("FullControl")
                }),
                "owner full-control ACL missing: {output}"
            );
            assert!(
                entries.iter().any(|entry| {
                    entry.starts_with("S-1-5-18|False|") && entry.ends_with("FullControl")
                }),
                "SYSTEM full-control ACL missing: {output}"
            );
        }
    }

    #[test]
    fn callback_parser_extracts_code_without_logging_it() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind callback fixture");
        let address = listener.local_addr().expect("callback address");
        let client = thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(address).expect("connect callback");
            std::io::Write::write_all(
                &mut stream,
                b"GET /auth/callback?state=expected&code=private-code&client_id=issued&scope=chatgpt.tokens.use.direct+email+offline_access+openid+profile+resource.invoke HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            )
            .expect("send callback");
        });
        let (mut stream, _) = listener.accept().expect("accept callback");
        let callback = parse_callback(&mut stream).expect("parse callback");
        client.join().expect("join callback client");
        assert_eq!(callback.state, "expected");
        assert_eq!(callback.code.as_deref(), Some("private-code"));
        assert_eq!(callback.client_id.as_deref(), Some("issued"));
    }

    #[test]
    fn callback_cancellation_interrupts_incomplete_headers_promptly() {
        use std::sync::Arc;
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};

        let (listener, port, _) = callback_listener().expect("create callback listener");
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancellation_flag = Arc::clone(&cancelled);
        let client = thread::spawn(move || {
            let mut stream =
                std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect callback");
            std::io::Write::write_all(
                &mut stream,
                b"GET /auth/callback?state=partial HTTP/1.1\r\nHost: 127.0.0.1\r\n",
            )
            .expect("send partial callback headers");
            thread::sleep(Duration::from_millis(80));
            cancellation_flag.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let result = await_callback(listener, port, &cancelled);
        client.join().expect("join partial callback client");
        assert!(matches!(
            result,
            Err(crate::error::SpecLoopError::Interrupted)
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn callback_parser_rejects_duplicate_parameters_and_request_body() {
        for request in [
            b"GET /auth/callback?state=a&state=b&code=c HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\nbody".as_slice(),
            b"GET /auth/callback?state=a&code=c HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 4\r\n\r\n".as_slice(),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind callback fixture");
            let address = listener.local_addr().expect("callback address");
            let request = request.to_vec();
            let client = thread::spawn(move || {
                let mut stream = std::net::TcpStream::connect(address).expect("connect callback");
                std::io::Write::write_all(&mut stream, &request).expect("send callback");
            });
            let (mut stream, _) = listener.accept().expect("accept callback");
            assert!(parse_callback(&mut stream).is_err());
            client.join().expect("join callback client");
        }
    }
}

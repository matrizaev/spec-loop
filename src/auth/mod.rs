use std::collections::BTreeSet;
use std::fs;
use std::future::Future;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use openidconnect::core::{
    CoreAuthenticationFlow, CoreClient, CoreIdTokenVerifier, CoreProviderMetadata,
    CoreTokenResponse,
};
use openidconnect::{
    AuthorizationCode, ClientId, CsrfToken, EndpointMaybeSet, EndpointNotSet, EndpointSet,
    IssuerUrl, Nonce, OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RefreshToken,
    Scope, TokenResponse,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

#[cfg(test)]
use self::callback::{CALLBACK_PATH, parse_callback};
use self::callback::{Callback, await_callback, callback_listener, validate_callback};
use self::registry::{normalize_host_id, registry_path, restrict_directory, restrict_file};
#[cfg(test)]
use self::registry::{
    project_boundary, registry_path_for_config_dir, validate_external_registry_path,
};
use crate::error::{Result, SpecLoopError, io_error};
use crate::runtime;

mod callback;
mod registry;
#[cfg(test)]
mod tests;

const ISSUER: &str = "https://auth.openai.com";
const DIRECT_SCOPE: &str = "chatgpt.tokens.use.direct";
const TOKEN_REFRESH_SKEW_SECONDS: u64 = 300;
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
        /// Selects automatic, existing, or new registration behavior.
        target: RegistrationTarget,
    },
    /// Report local credential status without revealing credentials.
    Status,
    /// Remove selected or active Spec Loop credentials.
    Logout {
        /// Opaque registration ID to remove locally.
        account: Option<String>,
    },
}

/// Registration selection requested at the authentication boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistrationTarget {
    /// Select the only saved registration automatically, if present.
    Automatic,
    /// Reauthorize the saved registration with this opaque ID.
    Existing(String),
    /// Begin a distinct dynamic client registration.
    New,
}

impl RegistrationTarget {
    /// Converts mutually exclusive CLI options into a validated selection.
    pub fn from_options(account: Option<String>, new_account: bool) -> Result<Self> {
        match (account, new_account) {
            (Some(_), true) => Err(SpecLoopError::Config(
                "--account and --new-account cannot be used together".to_owned(),
            )),
            (Some(account), false) => Ok(Self::Existing(account)),
            (None, true) => Ok(Self::New),
            (None, false) => Ok(Self::Automatic),
        }
    }
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

/// Dispatches profile-independent authentication commands.
pub fn run(command: AuthCommand, cancelled: &Arc<AtomicBool>) -> Result<()> {
    let path = registry_path()?;
    match command {
        AuthCommand::Login { target } => {
            runtime::block_on(login(path, target, Arc::clone(cancelled)))??;
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
    target: RegistrationTarget,
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
        target,
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
    target: RegistrationTarget,
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
    let existing = select_registration(&registry, &target)?.cloned();
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
        .is_none_or(|expires_at| expires_at <= now_unix.saturating_add(TOKEN_REFRESH_SKEW_SECONDS))
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

fn select_registration<'a>(
    registry: &'a Registry,
    target: &RegistrationTarget,
) -> Result<Option<&'a Registration>> {
    match target {
        RegistrationTarget::New => Ok(None),
        RegistrationTarget::Existing(account) => registry
            .registrations
            .iter()
            .find(|registration| registration.id == *account)
            .map(Some)
            .ok_or_else(|| {
                SpecLoopError::Config(format!("unknown ChatGPT registration ID {account:?}"))
            }),
        RegistrationTarget::Automatic => match registry.registrations.as_slice() {
            [] => Ok(None),
            [registration] => Ok(Some(registration)),
            _ => Err(SpecLoopError::Config(
                "multiple ChatGPT registrations exist; select `--account ID` or use `--new-account`"
                    .to_owned(),
            )),
        },
    }
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

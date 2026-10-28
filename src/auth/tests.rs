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
    Credential, CredentialStorage, CredentialWriteError, DIRECT_SCOPE, LoginServices, Registration,
    RegistrationPersistence, RegistrationTarget, RegistrationUpdate, Registry,
    active_access_token_at, authorization_details, await_callback, callback_listener, cancellable,
    credential_store_unavailable, expiry_timestamp, login_with_services, manual_authorization_url,
    parse_callback, present_authorization_url, project_boundary, refresh_credential,
    refresh_is_due, refreshed_credential, registration_id, registry_path_for_auth_dir,
    registry_path_for_config_dir, select_registration, validate_callback, validate_credential,
    validate_external_registry_path, write_test_credentials,
};
use crate::error::{Result, SpecLoopError};

#[test]
fn shared_token_scope_extraction_preserves_order_and_missing_scope() {
    for (scope, expected) in [
        (
            Some("profile chatgpt.tokens.use.direct openid"),
            vec!["profile", "chatgpt.tokens.use.direct", "openid"],
        ),
        (None, Vec::new()),
    ] {
        let mut value = json!({"access_token":"fixture", "token_type":"Bearer"});
        if let Some(scope) = scope {
            value["scope"] = json!(scope);
        }
        let token: openidconnect::core::CoreTokenResponse =
            serde_json::from_value(value).expect("parse token response");
        assert_eq!(super::token_scopes(&token), expected);
    }
}

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
    run_fake_login_at(
        temporary,
        path,
        token_response,
        expected_state,
        cancel_at_callback,
    )
}

fn run_fake_login_at(
    temporary: TempDir,
    path: std::path::PathBuf,
    token_response: FakeTokenResponse,
    expected_state: bool,
    cancel_at_callback: bool,
) -> FakeLoginRun {
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
        RegistrationTarget::Automatic,
        cancelled,
        LoginServices {
            issuer_url: &issuer,
            http: &http,
            open_browser: move |url: &str| {
                capture_authorization_url(url, &browser_capture);
                Ok::<(), ()>(())
            },
            receive_callback: move |listener: TcpListener,
                                    port: u16,
                                    cancelled: &Arc<AtomicBool>| {
                fake_callback(
                    listener,
                    port,
                    cancelled,
                    &callback_capture,
                    expected_state,
                    cancel_at_callback,
                )
            },
            save_registration: move |registry: &mut Registry,
                                     path: &Path,
                                     issuer: String,
                                     subject: String,
                                     client_id: String,
                                     credential: Credential,
                                     selected_id: Option<&str>| {
                assert_eq!(issuer, expected_issuer);
                save_test_registration(FakeRegistrationSave {
                    registry,
                    path,
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

fn assert_host_id_persisted(temporary: &TempDir, capture: &Mutex<Option<AuthorizationCapture>>) {
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
    path: &'a Path,
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
        path,
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
    let mut persisted = registry.clone();
    if let Some(stored) = persisted.registrations.last_mut() {
        stored.issuer = super::ISSUER.to_owned();
        stored.id = registration_id(&stored.issuer, &stored.subject, &stored.client_id)?;
        persisted.active_registration_id = Some(stored.id.clone());
    }
    persisted.save(path)?;
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
        include_str!("../../tests/fixtures/oidc-test-rsa-key.pem"),
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
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
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
            .is_some_and(|credential| credential.scopes.iter().any(|scope| scope == DIRECT_SCOPE))
    );
}

#[test]
fn successful_fake_login_persists_at_resolved_auth_directory() {
    let temporary = TempDir::new().expect("create custom auth directory fixture");
    let auth_dir = temporary.path().join("selected-auth");
    let path = registry_path_for_auth_dir(&auth_dir, None).expect("resolve override directory");
    let decoy = temporary.path().join("xdg/spec-loop/auth.json");
    let FakeLoginRun {
        result,
        server,
        temporary: _temporary,
        ..
    } = run_fake_login_at(
        temporary,
        path.clone(),
        FakeTokenResponse::Login {
            scopes: "openid offline_access chatgpt.tokens.use.direct".to_owned(),
            nonce_override: None,
            issuer_override: None,
        },
        true,
        false,
    );
    result.expect("complete fake OAuth login at custom path");
    assert_eq!(server.join().expect("join fake OIDC server").len(), 3);
    assert!(path.is_file());
    assert_eq!(path, auth_dir.join("auth.json"));
    assert!(!auth_dir.join("spec-loop/auth.json").exists());
    assert!(!decoy.exists());
    let persisted = Registry::load(&path).expect("reload custom auth registry");
    assert_eq!(persisted.registrations.len(), 1);
    assert!(persisted.active_registration_id.is_some());
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
    let error = crate::runtime::block_on(refresh_credential(&registration, &current, &cancelled))
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
        AuthUrl::new("https://auth.openai.com/authorize".to_owned()).expect("authorization URL"),
        JsonWebKeySetUrl::new("https://auth.openai.com/jwks".to_owned()).expect("JWKS URL"),
        vec![ResponseTypes::new(vec![CoreResponseType::Code])],
        vec![CoreSubjectIdentifierType::Public],
        vec![CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256],
        EmptyAdditionalProviderMetadata {},
    )
    .set_token_endpoint(Some(
        TokenUrl::new("https://auth.openai.com/token".to_owned()).expect("token URL"),
    ));
    CoreClient::from_provider_metadata(provider, ClientId::new("issued-client-id".to_owned()), None)
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
    assert!(authorization_parameter(&parameters, "nonce").is_some_and(|value| !value.is_empty()));
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
            id: registration_id(issuer, subject, client_id).expect("create test registration ID"),
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
    let update = test_registration_update(
        input.0,
        input.1,
        input.2,
        test_credential("file-token"),
        None,
    );
    RegistrationPersistence {
        save_registry: Registry::save,
        write_credential: |_: &str, _: &Credential| Err(CredentialWriteError::Unavailable),
        read_credential: |_: &Registration| Ok(None),
        restore_credential: |_: &str, _: Option<&Credential>| Ok(()),
    }
    .persist(&mut registry, &path, update)
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
    let update = test_registration_update(
        input.0,
        input.1,
        input.2,
        test_credential("must-not-fallback"),
        None,
    );
    let error = RegistrationPersistence {
        save_registry: Registry::save,
        write_credential: |_: &str, _: &Credential| {
            Err(CredentialWriteError::Failed(SpecLoopError::Workflow(
                "keyring write failed".to_owned(),
            )))
        },
        read_credential: |_: &Registration| Ok(None),
        restore_credential: |_: &str, _: Option<&Credential>| Ok(()),
    }
    .persist(&mut failed_registry, &failure_path, update)
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
    let update = test_registration_update(
        "https://auth.openai.com",
        "subject-new",
        "client-new",
        test_credential("new-token"),
        None,
    );
    let save_result = RegistrationPersistence {
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
    }
    .persist(&mut registry, &path, update);
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
    let update = test_registration_update(
        "https://auth.openai.com",
        "subject-old",
        "client-old",
        test_credential("new-token"),
        Some(&id),
    );
    let save_result = RegistrationPersistence {
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
    }
    .persist(&mut registry, &path, update);
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

#[test]
fn auth_directory_override_resolves_direct_auth_json_and_validates_project_boundary() {
    let temporary = TempDir::new().expect("create auth directory fixture");
    let selected = temporary.path().join("chosen");
    let expected = selected.join("auth.json");
    assert_eq!(
        registry_path_for_auth_dir(&selected, None).expect("resolve selected auth directory"),
        expected
    );
    assert!(!selected.exists());

    let project = TempDir::new().expect("create project boundary");
    std::fs::write(project.path().join(".spec-loop.toml"), "").expect("mark project boundary");
    let inside = project.path().join("credentials");
    let error = registry_path_for_auth_dir(&inside, Some(project.path()))
        .expect_err("reject custom path inside project");
    assert!(error.to_string().contains("outside the project directory"));
    assert!(!inside.exists());
}

#[test]
fn auth_directory_override_rejects_empty_and_relative_paths() {
    for directory in [Path::new(""), Path::new("relative-auth")] {
        let error = registry_path_for_auth_dir(directory, None)
            .expect_err("reject invalid authentication directory");
        assert!(error.to_string().contains("SPEC_LOOP_AUTH_DIR"));
        assert!(error.to_string().contains("absolute"));
    }
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
    let path = registry_path_for_auth_dir(&temporary.path().join("custom"), None)
        .expect("resolve custom auth directory");
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
fn refreshed_credentials_are_persisted_before_reuse() {
    let temporary = TempDir::new().expect("create credential fixture");
    let auth_dir = temporary.path().join("selected-auth");
    let path = registry_path_for_auth_dir(&auth_dir, None).expect("resolve custom auth directory");
    write_test_credentials(&path, vec![DIRECT_SCOPE.to_owned()]).expect("write credential fixture");
    let (mut registry, registration, _) =
        super::load_active_credential(&path).expect("load fixture credential");
    let refreshed = Credential {
        access_token: "persisted-new-access".to_owned(),
        refresh_token: Some("persisted-new-refresh".to_owned()),
        id_token: "fixture-id-token".to_owned(),
        scopes: vec![DIRECT_SCOPE.to_owned()],
        expires_at_unix: Some(u64::MAX),
    };

    super::store_refreshed_credential(&path, &mut registry, &registration, refreshed)
        .expect("persist refreshed file credential");

    let (_, _, persisted) =
        super::load_active_credential(&path).expect("reload refreshed credential");
    assert_eq!(persisted.access_token, "persisted-new-access");
    assert_eq!(
        persisted.refresh_token.as_deref(),
        Some("persisted-new-refresh")
    );
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
    assert!(refresh_is_due(&credential, 800));
    assert!(!refresh_is_due(&credential, 799));
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

#[test]
fn registration_targets_validate_options_and_select_without_mixed_states() {
    let empty = Registry {
        version: 1,
        host_id: "host".to_owned(),
        registrations: Vec::new(),
        active_registration_id: None,
    };
    assert_eq!(
        RegistrationTarget::from_options(None, false).expect("automatic selection"),
        RegistrationTarget::Automatic
    );
    assert_eq!(
        RegistrationTarget::from_options(Some("registration".to_owned()), false)
            .expect("existing registration selection"),
        RegistrationTarget::Existing("registration".to_owned())
    );
    assert_eq!(
        RegistrationTarget::from_options(None, true).expect("new registration selection"),
        RegistrationTarget::New
    );
    let mixed = RegistrationTarget::from_options(Some("registration".to_owned()), true)
        .expect_err("mixed registration options are invalid");
    assert!(mixed.to_string().contains("--account and --new-account"));
    assert!(
        select_registration(&empty, &RegistrationTarget::Automatic)
            .expect("empty automatic selection")
            .is_none()
    );
    assert!(
        select_registration(&empty, &RegistrationTarget::New)
            .expect("new registration selection")
            .is_none()
    );

    let registration = Registration {
        id: "registration".to_owned(),
        issuer: super::ISSUER.to_owned(),
        subject: "subject".to_owned(),
        client_id: "client".to_owned(),
        credential_storage: CredentialStorage::LoggedOut,
        fallback_credential: None,
    };
    let registry = Registry {
        registrations: vec![registration],
        ..empty
    };
    assert_eq!(
        select_registration(&registry, &RegistrationTarget::Automatic)
            .expect("single automatic selection")
            .map(|item| item.id.as_str()),
        Some("registration")
    );
    assert_eq!(
        select_registration(
            &registry,
            &RegistrationTarget::Existing("registration".to_owned())
        )
        .expect("existing selection")
        .map(|item| item.id.as_str()),
        Some("registration")
    );
    assert!(
        select_registration(
            &registry,
            &RegistrationTarget::Existing("missing".to_owned())
        )
        .is_err()
    );
    let mut registrations = registry.registrations.clone();
    registrations.extend(registry.registrations.clone());
    let multiple = Registry {
        registrations,
        ..registry
    };
    assert!(
        select_registration(&multiple, &RegistrationTarget::Automatic)
            .expect_err("ambiguous automatic selection")
            .to_string()
            .contains("multiple ChatGPT registrations")
    );
}

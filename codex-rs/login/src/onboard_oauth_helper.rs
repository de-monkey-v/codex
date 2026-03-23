//! Browser-based helper for onboarding login and Codex auth provisioning.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::channel;
use std::thread::JoinHandle;
use std::time::Duration;

use base64::Engine;
use codex_app_server_protocol::AuthMode;
use codex_client::build_reqwest_client_with_custom_ca;
use codex_core::auth::AuthDotJson;
use codex_core::config::find_codex_home;
use rand::RngCore;
use reqwest::Client;
use reqwest::Method;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use tiny_http::Header;
use tiny_http::Response;
use tiny_http::Server;
use tiny_http::StatusCode;
use url::Url;

const AUTH_ISSUER: &str = "https://auth.openai.com";
const PLATFORM_HYDRA_CLIENT_ID: &str = "app_2SKx67EdpoN0G6j64rFvigXD";
const PLATFORM_AUDIENCE: &str = "https://api.openai.com/v1";
const DEFAULT_API_BASE: &str = "https://api.openai.com";
const DEFAULT_CALLBACK_PORT: u16 = 5000;
const DEFAULT_CALLBACK_PATH: &str = "/auth/callback";
const DEFAULT_SCOPE: &str = "openid email profile offline_access";
const DEFAULT_APP: &str = "api";
const DEFAULT_USER_AGENT: &str = "OpenAI-Onboard-Auth-Script/1.0";
const DEFAULT_PROJECT_API_KEY_NAME: &str = "Codex CLI";
const DEFAULT_PROJECT_POLL_INTERVAL_SECONDS: u64 = 10;
const DEFAULT_PROJECT_POLL_TIMEOUT_SECONDS: u64 = 60;
const OAUTH_TIMEOUT_SECONDS: u64 = 15 * 60;
const HTTP_TIMEOUT_SECONDS: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiProvisionOptions {
    pub issuer: String,
    pub client_id: String,
    pub audience: String,
    pub api_base: String,
    pub app: String,
    pub callback_port: u16,
    pub scope: String,
    pub api_key_name: String,
    pub project_poll_interval_seconds: u64,
    pub project_poll_timeout_seconds: u64,
}

impl Default for ApiProvisionOptions {
    fn default() -> Self {
        Self {
            issuer: AUTH_ISSUER.to_string(),
            client_id: PLATFORM_HYDRA_CLIENT_ID.to_string(),
            audience: PLATFORM_AUDIENCE.to_string(),
            api_base: DEFAULT_API_BASE.to_string(),
            app: DEFAULT_APP.to_string(),
            callback_port: DEFAULT_CALLBACK_PORT,
            scope: DEFAULT_SCOPE.to_string(),
            api_key_name: DEFAULT_PROJECT_API_KEY_NAME.to_string(),
            project_poll_interval_seconds: DEFAULT_PROJECT_POLL_INTERVAL_SECONDS,
            project_poll_timeout_seconds: DEFAULT_PROJECT_POLL_TIMEOUT_SECONDS,
        }
    }
}

pub struct PendingApiProvisioning {
    client: Client,
    options: ApiProvisionOptions,
    redirect_uri: String,
    code_verifier: String,
    callback_server: LocalCallbackServer,
    auth_url: String,
}

impl PendingApiProvisioning {
    pub fn auth_url(&self) -> &str {
        &self.auth_url
    }

    pub fn open_browser(&self) -> bool {
        webbrowser::open(&self.auth_url).is_ok()
    }

    pub async fn finish(self) -> Result<ProvisionedApiKey, HelperError> {
        let code = self
            .callback_server
            .wait_for_code(Duration::from_secs(OAUTH_TIMEOUT_SECONDS))?;
        provision_from_authorization_code(
            &self.client,
            &self.options,
            &self.redirect_uri,
            &self.code_verifier,
            &code,
        )
        .await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionedApiKey {
    pub sensitive_id: String,
    pub organization_id: String,
    pub organization_title: Option<String>,
    pub default_project_id: String,
    pub default_project_title: Option<String>,
    pub project_api_key: String,
    pub access_token: String,
}

pub fn start_api_provisioning(
    options: ApiProvisionOptions,
) -> Result<PendingApiProvisioning, HelperError> {
    validate_api_provision_options(&options)?;
    let client = build_http_client()?;
    let pkce = generate_pkce();
    let state = generate_state();
    let callback_server =
        LocalCallbackServer::bind(options.callback_port, DEFAULT_CALLBACK_PATH, &state)?;
    let redirect_uri = callback_server.redirect_uri.clone();
    let auth_url = build_authorize_url(&options, &redirect_uri, &pkce, &state)?;

    Ok(PendingApiProvisioning {
        client,
        options,
        redirect_uri,
        code_verifier: pkce.code_verifier,
        callback_server,
        auth_url,
    })
}

pub async fn run_from_env() -> Result<(), HelperError> {
    match parse_args(std::env::args())? {
        ParseOutcome::Help(help) => {
            println!("{help}");
            Ok(())
        }
        ParseOutcome::Run(options) => {
            let auth_path = resolve_codex_auth_path(options.codex_auth_path.as_deref())?;
            let session = start_api_provisioning(options.api_provision_options())?;
            open_browser_or_print(session.auth_url());
            let provisioned = session.finish().await?;
            let codex_auth_synced = !options.skip_codex_auth_sync;
            if codex_auth_synced {
                sync_codex_api_key(&provisioned.project_api_key, &auth_path)?;
                eprintln!("Synced project API key to {}.", auth_path.display());
            } else {
                eprintln!("Skipping Codex auth sync.");
            }
            let output = ScriptOutput {
                sensitive_id: provisioned.sensitive_id,
                organization_id: provisioned.organization_id,
                organization_title: provisioned.organization_title,
                default_project_id: provisioned.default_project_id,
                default_project_title: provisioned.default_project_title,
                project_api_key: provisioned.project_api_key,
                codex_auth_path: auth_path.display().to_string(),
                codex_auth_synced,
                access_token: options
                    .include_access_token
                    .then_some(provisioned.access_token),
            };
            print_output(&output, options.output)?;
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliOptions {
    issuer: String,
    client_id: String,
    audience: String,
    api_base: String,
    app: String,
    callback_port: u16,
    scope: String,
    api_key_name: String,
    project_poll_interval_seconds: u64,
    project_poll_timeout_seconds: u64,
    codex_auth_path: Option<PathBuf>,
    skip_codex_auth_sync: bool,
    include_access_token: bool,
    output: OutputFormat,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            issuer: AUTH_ISSUER.to_string(),
            client_id: PLATFORM_HYDRA_CLIENT_ID.to_string(),
            audience: PLATFORM_AUDIENCE.to_string(),
            api_base: DEFAULT_API_BASE.to_string(),
            app: DEFAULT_APP.to_string(),
            callback_port: DEFAULT_CALLBACK_PORT,
            scope: DEFAULT_SCOPE.to_string(),
            api_key_name: DEFAULT_PROJECT_API_KEY_NAME.to_string(),
            project_poll_interval_seconds: DEFAULT_PROJECT_POLL_INTERVAL_SECONDS,
            project_poll_timeout_seconds: DEFAULT_PROJECT_POLL_TIMEOUT_SECONDS,
            codex_auth_path: None,
            skip_codex_auth_sync: false,
            include_access_token: false,
            output: OutputFormat::Json,
        }
    }
}

impl CliOptions {
    fn api_provision_options(&self) -> ApiProvisionOptions {
        ApiProvisionOptions {
            issuer: self.issuer.clone(),
            client_id: self.client_id.clone(),
            audience: self.audience.clone(),
            api_base: self.api_base.clone(),
            app: self.app.clone(),
            callback_port: self.callback_port,
            scope: self.scope.clone(),
            api_key_name: self.api_key_name.clone(),
            project_poll_interval_seconds: self.project_poll_interval_seconds,
            project_poll_timeout_seconds: self.project_poll_timeout_seconds,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Json,
    SensitiveId,
    ApiKey,
}

impl OutputFormat {
    fn parse(raw: &str) -> Result<Self, HelperError> {
        match raw {
            "json" => Ok(Self::Json),
            "sensitive_id" => Ok(Self::SensitiveId),
            "api_key" => Ok(Self::ApiKey),
            _ => Err(HelperError::message(format!(
                "invalid value for `--output`: `{raw}`"
            ))),
        }
    }
}

enum ParseOutcome {
    Help(String),
    Run(CliOptions),
}

fn parse_args<I>(args: I) -> Result<ParseOutcome, HelperError>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let program = args
        .next()
        .unwrap_or_else(|| "get_sensitive_id_via_codex_oauth".to_string());
    let mut options = CliOptions::default();
    let mut rest = args.peekable();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(ParseOutcome::Help(usage(&program))),
            "--issuer" => options.issuer = take_value(&mut rest, "--issuer")?,
            "--client-id" => options.client_id = take_value(&mut rest, "--client-id")?,
            "--audience" => options.audience = take_value(&mut rest, "--audience")?,
            "--api-base" => options.api_base = take_value(&mut rest, "--api-base")?,
            "--app" => options.app = take_value(&mut rest, "--app")?,
            "--callback-port" => {
                options.callback_port =
                    parse_u16(take_value(&mut rest, "--callback-port")?, "--callback-port")?
            }
            "--scope" => options.scope = take_value(&mut rest, "--scope")?,
            "--api-key-name" => options.api_key_name = take_value(&mut rest, "--api-key-name")?,
            "--project-poll-interval-seconds" => {
                options.project_poll_interval_seconds = parse_u64(
                    take_value(&mut rest, "--project-poll-interval-seconds")?,
                    "--project-poll-interval-seconds",
                )?
            }
            "--project-poll-timeout-seconds" => {
                options.project_poll_timeout_seconds = parse_u64(
                    take_value(&mut rest, "--project-poll-timeout-seconds")?,
                    "--project-poll-timeout-seconds",
                )?
            }
            "--codex-auth-path" => {
                options.codex_auth_path =
                    Some(PathBuf::from(take_value(&mut rest, "--codex-auth-path")?))
            }
            "--skip-codex-auth-sync" => options.skip_codex_auth_sync = true,
            "--include-access-token" => options.include_access_token = true,
            "--output" => {
                options.output = OutputFormat::parse(&take_value(&mut rest, "--output")?)?
            }
            _ => {
                return Err(HelperError::message(format!(
                    "unknown argument `{arg}`\n\n{}",
                    usage(&program)
                )));
            }
        }
    }
    validate_api_provision_options(&options.api_provision_options())?;
    Ok(ParseOutcome::Run(options))
}

fn validate_api_provision_options(options: &ApiProvisionOptions) -> Result<(), HelperError> {
    if options.project_poll_interval_seconds == 0 {
        return Err(HelperError::message(
            "--project-poll-interval-seconds must be greater than 0.".to_string(),
        ));
    }
    if options.project_poll_timeout_seconds == 0 {
        return Err(HelperError::message(
            "--project-poll-timeout-seconds must be greater than 0.".to_string(),
        ));
    }
    if options.api_key_name.trim().is_empty() {
        return Err(HelperError::message(
            "--api-key-name must not be empty.".to_string(),
        ));
    }
    Ok(())
}

fn usage(program: &str) -> String {
    format!(
        "Usage: {program} [OPTIONS]\n\n\
Options:\n\
  --issuer URL                          OAuth issuer base URL\n\
  --client-id ID                        Hydra client id to use\n\
  --audience URL                        OAuth audience\n\
  --api-base URL                        Base API URL for onboarding exchange\n\
  --app NAME                            `app` value for /dashboard/onboarding/login\n\
  --callback-port PORT                  Local callback port (default: {DEFAULT_CALLBACK_PORT})\n\
  --scope SCOPE                         OAuth scope string\n\
  --api-key-name NAME                   Provisioned project API key name\n\
  --project-poll-interval-seconds SEC   Delay between default-project checks\n\
  --project-poll-timeout-seconds SEC    Maximum wait for organization/project readiness\n\
  --codex-auth-path PATH                Explicit auth.json path\n\
  --skip-codex-auth-sync                Do not write the provisioned API key to auth.json\n\
  --include-access-token                Include the OAuth access token in JSON output\n\
  --output FORMAT                       One of: json, sensitive_id, api_key\n\
  -h, --help                            Show this help message"
    )
}

fn take_value<I>(args: &mut std::iter::Peekable<I>, flag: &str) -> Result<String, HelperError>
where
    I: Iterator<Item = String>,
{
    args.next()
        .ok_or_else(|| HelperError::message(format!("missing value for `{flag}`")))
}

fn parse_u16(raw: String, flag: &str) -> Result<u16, HelperError> {
    raw.parse::<u16>()
        .map_err(|err| HelperError::message(format!("invalid value for `{flag}`: {err}")))
}

fn parse_u64(raw: String, flag: &str) -> Result<u64, HelperError> {
    raw.parse::<u64>()
        .map_err(|err| HelperError::message(format!("invalid value for `{flag}`: {err}")))
}

#[derive(Debug, Clone)]
struct PkceCodes {
    code_verifier: String,
    code_challenge: String,
}

fn generate_pkce() -> PkceCodes {
    let mut bytes = [0u8; 64];
    rand::rng().fill_bytes(&mut bytes);
    let code_verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    PkceCodes {
        code_verifier,
        code_challenge,
    }
}

fn generate_state() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

struct LocalCallbackServer {
    redirect_uri: String,
    rx: Receiver<Result<String, String>>,
    server: Arc<Server>,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl LocalCallbackServer {
    fn bind(port: u16, callback_path: &str, expected_state: &str) -> Result<Self, HelperError> {
        let server = Arc::new(Server::http(format!("127.0.0.1:{port}")).map_err(|err| {
            HelperError::message(format!("failed to bind callback server: {err}"))
        })?);
        let actual_port = server
            .server_addr()
            .to_ip()
            .map(|addr| addr.port())
            .ok_or_else(|| {
                HelperError::message("unable to determine callback server port".to_string())
            })?;
        let (tx, rx) = channel();
        let callback_path = callback_path.to_string();
        let expected_state = expected_state.to_string();
        let redirect_uri = format!("http://localhost:{actual_port}{callback_path}");
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_for_thread = Arc::clone(&server);
        let shutdown_for_thread = Arc::clone(&shutdown);
        let callback_path_for_thread = callback_path;
        let worker = std::thread::spawn(move || {
            callback_loop(
                server_for_thread,
                shutdown_for_thread,
                tx,
                callback_path_for_thread,
                expected_state,
            );
        });
        Ok(Self {
            redirect_uri,
            rx,
            server,
            shutdown,
            worker: Some(worker),
        })
    }

    fn wait_for_code(self, timeout: Duration) -> Result<String, HelperError> {
        match self.rx.recv_timeout(timeout) {
            Ok(Ok(code)) => Ok(code),
            Ok(Err(message)) => Err(HelperError::message(message)),
            Err(_) => Err(HelperError::message(
                "OAuth login timed out waiting for the browser callback.".to_string(),
            )),
        }
    }
}

impl Drop for LocalCallbackServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.server.unblock();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn callback_loop(
    server: Arc<Server>,
    shutdown: Arc<AtomicBool>,
    tx: std::sync::mpsc::Sender<Result<String, String>>,
    callback_path: String,
    expected_state: String,
) {
    while !shutdown.load(Ordering::Relaxed) {
        let request = match server.recv_timeout(Duration::from_millis(200)) {
            Ok(Some(request)) => request,
            Ok(None) => continue,
            Err(_) => break,
        };
        let response = handle_callback_request(request.url(), &callback_path, &expected_state);
        let should_exit = response.result.is_some();
        let _ = request.respond(response.response);
        if let Some(result) = response.result {
            let _ = tx.send(result);
            break;
        }
        if should_exit {
            break;
        }
    }
}

struct CallbackResponse {
    response: Response<std::io::Cursor<Vec<u8>>>,
    result: Option<Result<String, String>>,
}

fn handle_callback_request(
    url_raw: &str,
    callback_path: &str,
    expected_state: &str,
) -> CallbackResponse {
    let parsed_url = match Url::parse(&format!("http://localhost{url_raw}")) {
        Ok(url) => url,
        Err(err) => {
            return CallbackResponse {
                response: html_response(400, format!("<h1>Bad Request</h1><p>{err}</p>")),
                result: None,
            };
        }
    };
    if parsed_url.path() != callback_path {
        return CallbackResponse {
            response: html_response(404, "<h1>Not Found</h1>".to_string()),
            result: None,
        };
    }

    let params: HashMap<String, String> = parsed_url.query_pairs().into_owned().collect();
    if params.get("state").map(String::as_str) != Some(expected_state) {
        return CallbackResponse {
            response: html_response(400, "<h1>State mismatch</h1>".to_string()),
            result: Some(Err("State mismatch in OAuth callback.".to_string())),
        };
    }
    if let Some(error_code) = params.get("error") {
        let message = oauth_callback_error_message(
            error_code,
            params.get("error_description").map(String::as_str),
        );
        return CallbackResponse {
            response: html_response(
                403,
                "<h1>Sign-in failed</h1><p>Return to your terminal.</p>".to_string(),
            ),
            result: Some(Err(message)),
        };
    }
    match params.get("code") {
        Some(code) if !code.is_empty() => CallbackResponse {
            response: html_response(
                200,
                "<h1>Sign-in complete</h1><p>You can return to your terminal.</p>".to_string(),
            ),
            result: Some(Ok(code.clone())),
        },
        _ => CallbackResponse {
            response: html_response(400, "<h1>Missing authorization code</h1>".to_string()),
            result: Some(Err(
                "Missing authorization code. Sign-in could not be completed.".to_string(),
            )),
        },
    }
}

fn html_response(status: u16, body: String) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut response = Response::from_string(body).with_status_code(StatusCode(status));
    if let Ok(header) = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]) {
        response = response.with_header(header);
    }
    response
}

fn oauth_callback_error_message(error_code: &str, error_description: Option<&str>) -> String {
    match error_description {
        Some(description) if !description.is_empty() => {
            format!("OAuth callback error ({error_code}): {description}")
        }
        _ => format!("OAuth callback error ({error_code})."),
    }
}

fn open_browser_or_print(auth_url: &str) {
    let opened = webbrowser::open(auth_url).is_ok();
    if opened {
        eprintln!(
            "Starting local login server.\nIf your browser did not open, navigate to this URL to authenticate:\n\n{auth_url}"
        );
    } else {
        eprintln!(
            "Starting local login server.\nOpen this URL in your browser to authenticate:\n\n{auth_url}"
        );
    }
}

fn build_authorize_url(
    options: &ApiProvisionOptions,
    redirect_uri: &str,
    pkce: &PkceCodes,
    state: &str,
) -> Result<String, HelperError> {
    let mut url = Url::parse(&format!(
        "{}/oauth/authorize",
        options.issuer.trim_end_matches('/')
    ))
    .map_err(|err| HelperError::message(format!("invalid issuer URL: {err}")))?;
    url.query_pairs_mut()
        .append_pair("audience", &options.audience)
        .append_pair("client_id", &options.client_id)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", &pkce.code_challenge)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &options.scope)
        .append_pair("state", state);
    Ok(url.to_string())
}

fn build_http_client() -> Result<Client, HelperError> {
    build_reqwest_client_with_custom_ca(
        reqwest::Client::builder().timeout(Duration::from_secs(HTTP_TIMEOUT_SECONDS)),
    )
    .map_err(|err| HelperError::message(format!("failed to build HTTP client: {err}")))
}

async fn provision_from_authorization_code(
    client: &Client,
    options: &ApiProvisionOptions,
    redirect_uri: &str,
    code_verifier: &str,
    code: &str,
) -> Result<ProvisionedApiKey, HelperError> {
    let tokens = exchange_authorization_code_for_tokens(
        client,
        &options.issuer,
        &options.client_id,
        redirect_uri,
        code_verifier,
        code,
    )
    .await?;
    let login = onboarding_login(
        client,
        &options.api_base,
        &options.app,
        &tokens.access_token,
    )
    .await?;
    let target = wait_for_default_project(
        client,
        &options.api_base,
        &login.user.session.sensitive_id,
        options.project_poll_interval_seconds,
        options.project_poll_timeout_seconds,
    )
    .await?;
    let api_key = create_project_api_key(
        client,
        &options.api_base,
        &login.user.session.sensitive_id,
        &target,
        &options.api_key_name,
    )
    .await?
    .key
    .sensitive_id;

    Ok(ProvisionedApiKey {
        sensitive_id: login.user.session.sensitive_id,
        organization_id: target.organization_id,
        organization_title: target.organization_title,
        default_project_id: target.project_id,
        default_project_title: target.project_title,
        project_api_key: api_key,
        access_token: tokens.access_token,
    })
}

async fn exchange_authorization_code_for_tokens(
    client: &Client,
    issuer: &str,
    client_id: &str,
    redirect_uri: &str,
    code_verifier: &str,
    code: &str,
) -> Result<OAuthTokens, HelperError> {
    let url = format!("{}/oauth/token", issuer.trim_end_matches('/'));
    execute_json(
        client
            .request(Method::POST, &url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, DEFAULT_USER_AGENT)
            .json(&serde_json::json!({
                "client_id": client_id,
                "code_verifier": code_verifier,
                "code": code,
                "grant_type": "authorization_code",
                "redirect_uri": redirect_uri,
            })),
        "POST",
        &url,
    )
    .await
}

async fn onboarding_login(
    client: &Client,
    api_base: &str,
    app: &str,
    access_token: &str,
) -> Result<OnboardingLoginResponse, HelperError> {
    let url = format!(
        "{}/dashboard/onboarding/login",
        api_base.trim_end_matches('/')
    );
    execute_json(
        client
            .request(Method::POST, &url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, DEFAULT_USER_AGENT)
            .bearer_auth(access_token)
            .json(&serde_json::json!({ "app": app })),
        "POST",
        &url,
    )
    .await
}

async fn list_organizations(
    client: &Client,
    api_base: &str,
    session_key: &str,
) -> Result<Vec<Organization>, HelperError> {
    let url = format!("{}/v1/organizations", api_base.trim_end_matches('/'));
    let response: DataList<Organization> = execute_json(
        client
            .request(Method::GET, &url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, DEFAULT_USER_AGENT)
            .bearer_auth(session_key),
        "GET",
        &url,
    )
    .await?;
    Ok(response.data)
}

async fn list_projects(
    client: &Client,
    api_base: &str,
    session_key: &str,
    organization_id: &str,
) -> Result<Vec<Project>, HelperError> {
    let url = format!(
        "{}/dashboard/organizations/{}/projects?detail=basic&limit=100",
        api_base.trim_end_matches('/'),
        urlencoding::encode(organization_id)
    );
    let response: DataList<Project> = execute_json(
        client
            .request(Method::GET, &url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, DEFAULT_USER_AGENT)
            .header("openai-organization", organization_id)
            .bearer_auth(session_key),
        "GET",
        &url,
    )
    .await?;
    Ok(response.data)
}

async fn wait_for_default_project(
    client: &Client,
    api_base: &str,
    session_key: &str,
    poll_interval_seconds: u64,
    timeout_seconds: u64,
) -> Result<ProvisioningTarget, HelperError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_seconds);
    loop {
        let organizations = list_organizations(client, api_base, session_key).await?;
        let last_state = if let Some(organization) = select_active_organization(&organizations) {
            let projects = list_projects(client, api_base, session_key, &organization.id).await?;
            if let Some(project) = find_default_project(&projects) {
                return Ok(ProvisioningTarget {
                    organization_id: organization.id.clone(),
                    organization_title: organization.title.clone(),
                    project_id: project.id.clone(),
                    project_title: project.title.clone(),
                });
            }
            format!(
                "organization `{}` exists, but no default project is ready yet (saw {} projects).",
                organization.id,
                projects.len()
            )
        } else {
            "no organization found".to_string()
        };

        if std::time::Instant::now() >= deadline {
            return Err(HelperError::message(format!(
                "Timed out waiting for an organization and default project. Last observed state: {last_state}"
            )));
        }
        let remaining_seconds = deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_secs();
        let sleep_seconds = poll_interval_seconds.min(remaining_seconds.max(1));
        std::thread::sleep(Duration::from_secs(sleep_seconds));
    }
}

fn select_active_organization(organizations: &[Organization]) -> Option<&Organization> {
    organizations
        .iter()
        .find(|organization| organization.is_default)
        .or_else(|| {
            organizations
                .iter()
                .find(|organization| organization.personal)
        })
        .or_else(|| organizations.first())
}

fn find_default_project(projects: &[Project]) -> Option<&Project> {
    projects.iter().find(|project| project.is_initial)
}

async fn create_project_api_key(
    client: &Client,
    api_base: &str,
    session_key: &str,
    target: &ProvisioningTarget,
    key_name: &str,
) -> Result<CreateApiKeyResponse, HelperError> {
    let url = format!(
        "{}/dashboard/organizations/{}/projects/{}/api_keys",
        api_base.trim_end_matches('/'),
        urlencoding::encode(&target.organization_id),
        urlencoding::encode(&target.project_id)
    );
    execute_json(
        client
            .request(Method::POST, &url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, DEFAULT_USER_AGENT)
            .bearer_auth(session_key)
            .json(&serde_json::json!({
                "action": "create",
                "name": key_name,
            })),
        "POST",
        &url,
    )
    .await
}

async fn execute_json<T>(
    request: reqwest::RequestBuilder,
    method: &str,
    url: &str,
) -> Result<T, HelperError>
where
    T: for<'de> Deserialize<'de>,
{
    let response = request
        .send()
        .await
        .map_err(|err| HelperError::message(format!("Network error calling {url}: {err}")))?;
    let status = response.status();
    let body = response.bytes().await.map_err(|err| {
        HelperError::message(format!("Failed reading response from {url}: {err}"))
    })?;
    if !status.is_success() {
        return Err(HelperError::api(
            format!("{method} {url} failed with HTTP {status}"),
            String::from_utf8_lossy(&body).into_owned(),
        ));
    }
    serde_json::from_slice(&body)
        .map_err(|err| HelperError::message(format!("{url} returned invalid JSON: {err}")))
}

fn resolve_codex_auth_path(explicit: Option<&Path>) -> Result<PathBuf, HelperError> {
    match explicit {
        Some(path) => Ok(path.to_path_buf()),
        None => Ok(find_codex_home()
            .map_err(|err| HelperError::message(format!("failed to resolve CODEX_HOME: {err}")))?
            .join("auth.json")),
    }
}

fn sync_codex_api_key(api_key: &str, auth_path: &Path) -> Result<(), HelperError> {
    if let Some(parent) = auth_path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            HelperError::message(format!(
                "Failed to create auth directory {}: {err}",
                parent.display()
            ))
        })?;
    }
    let auth = AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some(api_key.to_string()),
        tokens: None,
        last_refresh: None,
    };
    let json = format!(
        "{}\n",
        serde_json::to_string_pretty(&auth).map_err(|err| {
            HelperError::message(format!("failed to serialize auth.json contents: {err}"))
        })?
    );
    let mut options = OpenOptions::new();
    options.truncate(true).write(true).create(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options.open(auth_path).map_err(|err| {
        HelperError::message(format!(
            "Failed to open {} for writing: {err}",
            auth_path.display()
        ))
    })?;
    file.write_all(json.as_bytes()).map_err(|err| {
        HelperError::message(format!("Failed to write {}: {err}", auth_path.display()))
    })?;
    file.flush().map_err(|err| {
        HelperError::message(format!("Failed to flush {}: {err}", auth_path.display()))
    })
}

fn print_output(output: &ScriptOutput, format: OutputFormat) -> Result<(), HelperError> {
    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(output).map_err(|err| {
                    HelperError::message(format!("failed to serialize output: {err}"))
                })?
            );
        }
        OutputFormat::SensitiveId => println!("{}", output.sensitive_id),
        OutputFormat::ApiKey => println!("{}", output.project_api_key),
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ScriptOutput {
    sensitive_id: String,
    organization_id: String,
    organization_title: Option<String>,
    default_project_id: String,
    default_project_title: Option<String>,
    project_api_key: String,
    codex_auth_path: String,
    codex_auth_synced: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    access_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthTokens {
    #[serde(rename = "id_token")]
    _id_token: String,
    access_token: String,
    #[serde(rename = "refresh_token")]
    _refresh_token: String,
}

#[derive(Debug, Deserialize)]
struct OnboardingLoginResponse {
    user: OnboardingUser,
}

#[derive(Debug, Deserialize)]
struct OnboardingUser {
    session: OnboardingSession,
}

#[derive(Debug, Deserialize)]
struct OnboardingSession {
    sensitive_id: String,
}

#[derive(Debug, Deserialize)]
struct DataList<T> {
    data: Vec<T>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct Organization {
    id: String,
    title: Option<String>,
    #[serde(default)]
    is_default: bool,
    #[serde(default)]
    personal: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct Project {
    id: String,
    title: Option<String>,
    #[serde(default)]
    is_initial: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProvisioningTarget {
    organization_id: String,
    organization_title: Option<String>,
    project_id: String,
    project_title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateApiKeyResponse {
    key: CreatedApiKey,
}

#[derive(Debug, Deserialize)]
struct CreatedApiKey {
    sensitive_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperError {
    message: String,
    body: Option<String>,
}

impl HelperError {
    fn message(message: String) -> Self {
        Self {
            message,
            body: None,
        }
    }

    fn api(message: String, body: String) -> Self {
        Self {
            message,
            body: Some(body),
        }
    }

    pub fn body(&self) -> Option<&str> {
        self.body.as_deref()
    }
}

impl std::fmt::Display for HelperError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HelperError {}

#[cfg(test)]
#[path = "onboard_oauth_helper_tests.rs"]
mod tests;

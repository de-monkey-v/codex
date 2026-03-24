use std::path::Path;
use std::path::PathBuf;

use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_client::TypedRequestError;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::LoginAccountParams;
use codex_app_server_protocol::LoginAccountResponse;
use codex_app_server_protocol::RequestId;
use codex_core::auth::read_openai_api_key_from_env;
use codex_login::ApiProvisionOptions;
use codex_login::OPENAI_API_KEY_ENV_VAR;
use codex_login::PendingApiProvisioning;
use codex_login::ProvisionedApiKey;
use codex_login::start_api_provisioning;
use codex_login::upsert_dotenv_api_key;
use codex_login::validate_dotenv_target;
use codex_protocol::config_types::ForcedLoginMethod;
use uuid::Uuid;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::history_cell;
use crate::history_cell::PlainHistoryCell;

pub(crate) struct ApiProvisionStartMessage {
    pub(crate) message: String,
    pub(crate) hint: Option<String>,
}

pub(crate) fn start_command(
    app_event_tx: AppEventSender,
    request_handle: AppServerRequestHandle,
    cwd: PathBuf,
    forced_login_method: Option<ForcedLoginMethod>,
) -> Result<ApiProvisionStartMessage, String> {
    if read_openai_api_key_from_env().is_some() {
        return Ok(existing_shell_api_key_message());
    }

    let dotenv_path = cwd.join(".env");
    let start_hint = format!(
        "Codex will save {OPENAI_API_KEY_ENV_VAR} to {path} and hot-apply it here when allowed.",
        path = dotenv_path.display()
    );
    validate_dotenv_target(&dotenv_path).map_err(|err| {
        format!(
            "Unable to prepare {} for {OPENAI_API_KEY_ENV_VAR}: {err}",
            dotenv_path.display(),
        )
    })?;

    let session = start_api_provisioning(ApiProvisionOptions::default())
        .map_err(|err| format!("Failed to start API provisioning: {err}"))?;
    if !session.open_browser() {
        return Err(
            "Failed to open your browser for API provisioning. Try again from a desktop session or use the helper binary."
                .to_string(),
        );
    }

    tokio::spawn(async move {
        let cell =
            complete_command(session, dotenv_path, forced_login_method, request_handle).await;
        app_event_tx.send(AppEvent::InsertHistoryCell(Box::new(cell)));
    });

    Ok(ApiProvisionStartMessage {
        message: "Opening your browser to provision a project API key.".to_string(),
        hint: Some(start_hint),
    })
}

fn existing_shell_api_key_message() -> ApiProvisionStartMessage {
    ApiProvisionStartMessage {
        message: format!(
            "{OPENAI_API_KEY_ENV_VAR} is already set in this Codex session; skipping API provisioning."
        ),
        hint: Some(format!(
            "This Codex session already inherited {OPENAI_API_KEY_ENV_VAR} from its shell environment. Unset it and run /api-provision again if you want Codex to provision and save a different key."
        )),
    }
}

async fn complete_command(
    session: PendingApiProvisioning,
    dotenv_path: PathBuf,
    forced_login_method: Option<ForcedLoginMethod>,
    request_handle: AppServerRequestHandle,
) -> PlainHistoryCell {
    let provisioned = match session.finish().await {
        Ok(provisioned) => provisioned,
        Err(err) => {
            return history_cell::new_error_event(format!("API provisioning failed: {err}"));
        }
    };

    if let Err(err) = upsert_dotenv_api_key(&dotenv_path, &provisioned.project_api_key) {
        return history_cell::new_error_event(format!(
            "Provisioning completed, but Codex could not update {}: {err}",
            dotenv_path.display()
        ));
    }

    success_cell(
        &provisioned,
        &dotenv_path,
        live_apply_api_key(
            forced_login_method,
            &provisioned.project_api_key,
            request_handle,
        )
        .await,
    )
}

async fn live_apply_api_key(
    forced_login_method: Option<ForcedLoginMethod>,
    api_key: &str,
    request_handle: AppServerRequestHandle,
) -> LiveApplyOutcome {
    if matches!(forced_login_method, Some(ForcedLoginMethod::Chatgpt)) {
        return LiveApplyOutcome::Skipped(chatgpt_required_message());
    }

    let request = ClientRequest::LoginAccount {
        request_id: api_provision_request_id(),
        params: LoginAccountParams::EphemeralApiKey {
            api_key: api_key.to_string(),
        },
    };

    match request_handle
        .request_typed::<LoginAccountResponse>(request)
        .await
    {
        Ok(LoginAccountResponse::ApiKey {}) => LiveApplyOutcome::Applied,
        Ok(other) => LiveApplyOutcome::Failed(format!(
            "unexpected account/login/start response: {other:?}"
        )),
        Err(TypedRequestError::Server { source, .. }) => {
            if let Some(reason) = skip_reason_for_live_apply_message(&source.message) {
                LiveApplyOutcome::Skipped(reason)
            } else {
                LiveApplyOutcome::Failed(source.message)
            }
        }
        Err(err) => LiveApplyOutcome::Failed(err.to_string()),
    }
}

fn skip_reason_for_live_apply_message(message: &str) -> Option<String> {
    if message == "API key login is disabled. Use ChatGPT login instead." {
        return Some(chatgpt_required_message());
    }

    if message.starts_with("External auth is active.") {
        return Some(format!(
            "Saved {OPENAI_API_KEY_ENV_VAR} to .env, but left this session unchanged because external auth is currently active here."
        ));
    }

    None
}

fn chatgpt_required_message() -> String {
    format!(
        "Saved {OPENAI_API_KEY_ENV_VAR} to .env, but left this session unchanged because ChatGPT login is required here."
    )
}

fn api_provision_request_id() -> RequestId {
    RequestId::String(Uuid::new_v4().to_string())
}

fn success_cell(
    provisioned: &ProvisionedApiKey,
    dotenv_path: &Path,
    live_apply_outcome: LiveApplyOutcome,
) -> PlainHistoryCell {
    let organization = provisioned
        .organization_title
        .clone()
        .unwrap_or_else(|| provisioned.organization_id.clone());
    let project = provisioned
        .default_project_title
        .clone()
        .unwrap_or_else(|| provisioned.default_project_id.clone());
    let hint = match live_apply_outcome {
        LiveApplyOutcome::Applied => Some(
            "Updated this session to use the newly provisioned API key without touching auth.json."
                .to_string(),
        ),
        LiveApplyOutcome::Skipped(reason) => Some(reason),
        LiveApplyOutcome::Failed(err) => Some(format!(
            "Saved {OPENAI_API_KEY_ENV_VAR} to {}, but could not hot-apply it in this session: {err}",
            dotenv_path.display(),
        )),
    };

    history_cell::new_info_event(
        format!(
            "Provisioned an API key for {organization} / {project} and saved {OPENAI_API_KEY_ENV_VAR} to {}.",
            dotenv_path.display()
        ),
        hint,
    )
}

enum LiveApplyOutcome {
    Applied,
    Skipped(String),
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_cell::HistoryCell;
    use pretty_assertions::assert_eq;

    #[test]
    fn success_cell_renders_expected_copy() {
        let cell = success_cell(
            &ProvisionedApiKey {
                sensitive_id: "session-123".to_string(),
                organization_id: "org-default".to_string(),
                organization_title: Some("Default Org".to_string()),
                default_project_id: "proj-default".to_string(),
                default_project_title: Some("Default Project".to_string()),
                project_api_key: "sk-proj-123".to_string(),
                access_token: "oauth-access-123".to_string(),
            },
            Path::new("/tmp/workspace/.env"),
            LiveApplyOutcome::Applied,
        );

        assert_eq!(
            render_cell(&cell),
            "• Provisioned an API key for Default Org / Default Project and saved OPENAI_API_KEY to /tmp/workspace/.env. Updated this session to use the newly provisioned API key without touching auth.json."
        );
    }

    #[test]
    fn success_cell_renders_skip_reason() {
        let cell = success_cell(
            &ProvisionedApiKey {
                sensitive_id: "session-123".to_string(),
                organization_id: "org-default".to_string(),
                organization_title: None,
                default_project_id: "proj-default".to_string(),
                default_project_title: None,
                project_api_key: "sk-proj-123".to_string(),
                access_token: "oauth-access-123".to_string(),
            },
            Path::new("/tmp/workspace/.env"),
            LiveApplyOutcome::Skipped(chatgpt_required_message()),
        );

        assert_eq!(
            render_cell(&cell),
            "• Provisioned an API key for org-default / proj-default and saved OPENAI_API_KEY to /tmp/workspace/.env. Saved OPENAI_API_KEY to .env, but left this session unchanged because ChatGPT login is required here."
        );
    }

    #[test]
    fn existing_shell_api_key_message_mentions_openai_api_key() {
        let message = existing_shell_api_key_message();

        assert_eq!(
            message.message,
            "OPENAI_API_KEY is already set in this Codex session; skipping API provisioning."
        );
        assert_eq!(
            message.hint,
            Some(
                "This Codex session already inherited OPENAI_API_KEY from its shell environment. Unset it and run /api-provision again if you want Codex to provision and save a different key.".to_string()
            )
        );
    }

    #[test]
    fn external_auth_live_apply_message_is_treated_as_skip() {
        assert_eq!(
            skip_reason_for_live_apply_message(
                "External auth is active. Use account/login/start (chatgptAuthTokens) to update it or account/logout to clear it."
            ),
            Some(
                "Saved OPENAI_API_KEY to .env, but left this session unchanged because external auth is currently active here."
                    .to_string()
            )
        );
    }

    fn render_cell(cell: &PlainHistoryCell) -> String {
        cell.display_lines(120)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

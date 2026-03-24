use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use codex_core::AuthManager;
use codex_core::auth::AuthCredentialsStoreMode;
use codex_core::auth::login_with_api_key;
use codex_core::auth::read_openai_api_key_from_env;
use codex_login::ApiProvisionOptions;
use codex_login::OPENAI_API_KEY_ENV_VAR;
use codex_login::PendingApiProvisioning;
use codex_login::ProvisionedApiKey;
use codex_login::start_api_provisioning;
use codex_login::upsert_dotenv_api_key;
use codex_login::validate_dotenv_target;
use codex_protocol::config_types::ForcedLoginMethod;

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
    auth_manager: Arc<AuthManager>,
    codex_home: PathBuf,
    cwd: PathBuf,
    forced_login_method: Option<ForcedLoginMethod>,
) -> Result<ApiProvisionStartMessage, String> {
    if read_openai_api_key_from_env().is_some() {
        return Ok(existing_shell_api_key_message());
    }

    let dotenv_path = cwd.join(".env.local");
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

    let app_event_tx_for_task = app_event_tx;
    let dotenv_path_for_task = dotenv_path;
    tokio::spawn(async move {
        let cell = complete_command(
            session,
            dotenv_path_for_task,
            codex_home,
            forced_login_method,
            auth_manager,
        )
        .await;
        app_event_tx_for_task.send(AppEvent::InsertHistoryCell(Box::new(cell)));
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
    codex_home: PathBuf,
    forced_login_method: Option<ForcedLoginMethod>,
    auth_manager: Arc<AuthManager>,
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
            &codex_home,
            &provisioned.project_api_key,
            auth_manager,
        ),
    )
}

fn live_apply_api_key(
    forced_login_method: Option<ForcedLoginMethod>,
    codex_home: &Path,
    api_key: &str,
    auth_manager: Arc<AuthManager>,
) -> LiveApplyOutcome {
    if matches!(forced_login_method, Some(ForcedLoginMethod::Chatgpt)) {
        return LiveApplyOutcome::Skipped(format!(
            "Saved {OPENAI_API_KEY_ENV_VAR} to .env.local, but left this session unchanged because ChatGPT login is required here."
        ));
    }

    match login_with_api_key(codex_home, api_key, AuthCredentialsStoreMode::Ephemeral) {
        Ok(()) => {
            auth_manager.reload();
            LiveApplyOutcome::Applied
        }
        Err(err) => LiveApplyOutcome::Failed(err.to_string()),
    }
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
    use insta::assert_snapshot;

    #[test]
    fn success_cell_snapshot() {
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
            Path::new("/tmp/workspace/.env.local"),
            LiveApplyOutcome::Applied,
        );

        assert_snapshot!(render_cell(&cell));
    }

    #[test]
    fn success_cell_snapshot_when_live_apply_is_skipped() {
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
            Path::new("/tmp/workspace/.env.local"),
            LiveApplyOutcome::Skipped(
                "Saved OPENAI_API_KEY to .env.local, but left this session unchanged because ChatGPT login is required here."
                    .to_string(),
            ),
        );

        assert_snapshot!(render_cell(&cell));
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

    fn render_cell(cell: &PlainHistoryCell) -> String {
        cell.display_lines(120)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

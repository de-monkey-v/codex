use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::tempdir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_json;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::query_param;

#[test]
fn select_active_organization_prefers_default_then_personal_then_first() {
    let organizations = vec![
        Organization {
            id: "org-first".to_string(),
            title: Some("First".to_string()),
            is_default: false,
            personal: false,
        },
        Organization {
            id: "org-personal".to_string(),
            title: Some("Personal".to_string()),
            is_default: false,
            personal: true,
        },
        Organization {
            id: "org-default".to_string(),
            title: Some("Default".to_string()),
            is_default: true,
            personal: false,
        },
    ];

    let selected = select_active_organization(&organizations);

    assert_eq!(selected, organizations.get(2));
}

#[test]
fn find_default_project_returns_initial_project() {
    let projects = vec![
        Project {
            id: "proj-secondary".to_string(),
            title: Some("Secondary".to_string()),
            is_initial: false,
        },
        Project {
            id: "proj-default".to_string(),
            title: Some("Default".to_string()),
            is_initial: true,
        },
    ];

    let selected = find_default_project(&projects);

    assert_eq!(selected, projects.get(1));
}

#[test]
fn sync_codex_api_key_writes_expected_auth_json() {
    let temp_dir = tempdir().expect("tempdir");
    let auth_path = temp_dir.path().join("codex").join("auth.json");

    sync_codex_api_key("sk-test-key", &auth_path).expect("sync auth");

    let written = std::fs::read_to_string(&auth_path).expect("read auth");
    let parsed: AuthDotJson = serde_json::from_str(&written).expect("parse auth");
    assert_eq!(
        parsed,
        AuthDotJson {
            auth_mode: Some(AuthMode::ApiKey),
            openai_api_key: Some("sk-test-key".to_string()),
            tokens: None,
            last_refresh: None,
        }
    );
}

#[tokio::test]
async fn provision_from_authorization_code_provisions_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_json(json!({
            "client_id": "client-123",
            "code_verifier": "verifier-123",
            "code": "auth-code-123",
            "grant_type": "authorization_code",
            "redirect_uri": "http://localhost:5000/auth/callback",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id_token": "id-token-123",
            "access_token": "oauth-access-123",
            "refresh_token": "oauth-refresh-123",
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/dashboard/onboarding/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "user": {
                "session": {
                    "sensitive_id": "session-123",
                }
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/organizations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {
                    "id": "org-default",
                    "title": "Default Org",
                    "is_default": true,
                }
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/dashboard/organizations/org-default/projects"))
        .and(query_param("detail", "basic"))
        .and(query_param("limit", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {
                    "id": "proj-default",
                    "title": "Default Project",
                    "is_initial": true,
                }
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(
            "/dashboard/organizations/org-default/projects/proj-default/api_keys",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "key": {
                "sensitive_id": "sk-proj-123",
            }
        })))
        .mount(&server)
        .await;

    let options = ApiProvisionOptions {
        issuer: server.uri(),
        client_id: "client-123".to_string(),
        audience: PLATFORM_AUDIENCE.to_string(),
        api_base: server.uri(),
        app: DEFAULT_APP.to_string(),
        callback_port: DEFAULT_CALLBACK_PORT,
        scope: DEFAULT_SCOPE.to_string(),
        api_key_name: DEFAULT_PROJECT_API_KEY_NAME.to_string(),
        project_poll_interval_seconds: 1,
        project_poll_timeout_seconds: 5,
    };
    let client = build_http_client().expect("client");

    let output = provision_from_authorization_code(
        &client,
        &options,
        "http://localhost:5000/auth/callback",
        "verifier-123",
        "auth-code-123",
    )
    .await
    .expect("provision");

    assert_eq!(
        output,
        ProvisionedApiKey {
            sensitive_id: "session-123".to_string(),
            organization_id: "org-default".to_string(),
            organization_title: Some("Default Org".to_string()),
            default_project_id: "proj-default".to_string(),
            default_project_title: Some("Default Project".to_string()),
            project_api_key: "sk-proj-123".to_string(),
            access_token: "oauth-access-123".to_string(),
        }
    );
}

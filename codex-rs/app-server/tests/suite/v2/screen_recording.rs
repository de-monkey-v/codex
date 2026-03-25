use anyhow::Context;
use anyhow::Result;
use app_test_support::McpProcess;
use app_test_support::to_response;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ConfigValueWriteParams;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::MergeStrategy;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ScreenRecordingPauseResponse;
use codex_app_server_protocol::ScreenRecordingReadResponse;
use codex_app_server_protocol::ScreenRecordingResumeResponse;
use codex_app_server_protocol::ScreenRecordingState;
use codex_app_server_protocol::ScreenRecordingStatus;
use codex_app_server_protocol::ScreenRecordingStatusUpdatedNotification;
use pretty_assertions::assert_eq;
use serde_json::json;
use serial_test::serial;
use std::path::Path;
use std::path::PathBuf;
use tempfile::TempDir;
use tokio::time::Instant;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn write_config(codex_home: &TempDir, contents: &str) -> Result<()> {
    Ok(std::fs::write(
        codex_home.path().join("config.toml"),
        contents,
    )?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(screen_recording)]
async fn screen_recording_persists_segment_files_end_to_end() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_config(
        &codex_home,
        r#"
[features]
screen_recording = true

[otel]
exporter = "none"
trace_exporter = "none"
metrics_exporter = "none"

[recording.screen]
enabled = true
"#,
    )?;

    let mut mcp = McpProcess::new_with_env(
        codex_home.path(),
        &[("CODEX_SCREEN_RECORDING_FAKE", Some("1"))],
    )
    .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.initialize_with_capabilities(
            ClientInfo {
                name: "codex_vscode".to_string(),
                title: Some("Codex VS Code Extension".to_string()),
                version: "0.1.0".to_string(),
            },
            Some(InitializeCapabilities {
                experimental_api: true,
                opt_out_notification_methods: None,
            }),
        ),
    )
    .await??;

    let running = wait_for_status(&mut mcp, ScreenRecordingState::Running).await?;
    let (segment_path, manifest_path, manifest) =
        wait_for_segment_artifacts(running.storage_path.as_path()).await?;
    let segment_bytes = std::fs::read(&segment_path)?;
    assert!(!segment_bytes.is_empty());
    #[cfg(target_os = "macos")]
    {
        assert!(segment_bytes.len() >= 8);
        assert_eq!(segment_bytes[4..8], *b"ftyp");
    }
    assert_eq!(
        manifest
            .get("display_id")
            .and_then(serde_json::Value::as_str),
        Some("fake-display-1")
    );
    assert!(
        manifest
            .get("frame_count")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|frame_count| frame_count > 0),
        "expected a non-zero frame_count in {manifest_path:?}: {manifest}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(screen_recording)]
async fn screen_recording_autostarts_and_supports_pause_resume() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_config(
        &codex_home,
        r#"
[features]
screen_recording = true

[otel]
exporter = "none"
trace_exporter = "none"
metrics_exporter = "none"

[recording.screen]
enabled = true
"#,
    )?;

    let mut mcp = McpProcess::new_with_env(
        codex_home.path(),
        &[("CODEX_SCREEN_RECORDING_FAKE", Some("1"))],
    )
    .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.initialize_with_capabilities(
            ClientInfo {
                name: "codex_vscode".to_string(),
                title: Some("Codex VS Code Extension".to_string()),
                version: "0.1.0".to_string(),
            },
            Some(InitializeCapabilities {
                experimental_api: true,
                opt_out_notification_methods: None,
            }),
        ),
    )
    .await??;

    let running = wait_for_status(&mut mcp, ScreenRecordingState::Running).await?;
    assert_eq!(running.captured_display_count, 1);
    assert!(running.storage_path.as_path().exists());

    let pause_request_id = mcp.send_screen_recording_pause_request().await?;
    let pause_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(pause_request_id)),
    )
    .await??;
    let pause: ScreenRecordingPauseResponse = to_response(pause_response)?;
    assert_eq!(pause.status.state, ScreenRecordingState::Paused);
    assert!(pause.status.paused);
    let paused_notification = read_status_notification(&mut mcp, |status| {
        status.state == ScreenRecordingState::Paused && status.paused
    })
    .await?;
    assert_eq!(
        paused_notification.status.state,
        ScreenRecordingState::Paused
    );

    let resume_request_id = mcp.send_screen_recording_resume_request().await?;
    let resume_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(resume_request_id)),
    )
    .await??;
    let resume: ScreenRecordingResumeResponse = to_response(resume_response)?;
    assert!(
        matches!(
            resume.status.state,
            ScreenRecordingState::Starting | ScreenRecordingState::Running
        ),
        "unexpected resume state: {:?}",
        resume.status.state
    );
    let resumed = wait_for_status(&mut mcp, ScreenRecordingState::Running).await?;
    assert_eq!(resumed.captured_display_count, 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(screen_recording)]
async fn screen_recording_disable_via_config_write_stops_and_purges() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_config(
        &codex_home,
        r#"
[features]
screen_recording = true

[recording.screen]
enabled = true
"#,
    )?;

    let mut mcp = McpProcess::new_with_env(
        codex_home.path(),
        &[("CODEX_SCREEN_RECORDING_FAKE", Some("1"))],
    )
    .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.initialize_with_capabilities(
            ClientInfo {
                name: "codex_vscode".to_string(),
                title: Some("Codex VS Code Extension".to_string()),
                version: "0.1.0".to_string(),
            },
            Some(InitializeCapabilities {
                experimental_api: true,
                opt_out_notification_methods: None,
            }),
        ),
    )
    .await??;

    let running = wait_for_status(&mut mcp, ScreenRecordingState::Running).await?;
    let request_id = mcp
        .send_config_value_write_request(ConfigValueWriteParams {
            key_path: "recording.screen.enabled".to_string(),
            value: json!(false),
            merge_strategy: MergeStrategy::Replace,
            file_path: None,
            expected_version: None,
        })
        .await?;
    let _: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;

    let notification = read_status_notification(&mut mcp, |status| {
        status.state == ScreenRecordingState::Disabled
    })
    .await?;
    assert_eq!(notification.status.state, ScreenRecordingState::Disabled);

    let disabled = read_screen_recording_status(&mut mcp).await?;
    assert_eq!(disabled.state, ScreenRecordingState::Disabled);
    assert!(!running.storage_path.as_path().exists());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(screen_recording)]
async fn screen_recording_status_updates_can_be_opted_out() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_config(
        &codex_home,
        r#"
[features]
screen_recording = true

[recording.screen]
enabled = true
"#,
    )?;

    let mut mcp = McpProcess::new_with_env(
        codex_home.path(),
        &[("CODEX_SCREEN_RECORDING_FAKE", Some("1"))],
    )
    .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.initialize_with_capabilities(
            ClientInfo {
                name: "codex_vscode".to_string(),
                title: Some("Codex VS Code Extension".to_string()),
                version: "0.1.0".to_string(),
            },
            Some(InitializeCapabilities {
                experimental_api: true,
                opt_out_notification_methods: Some(vec![
                    "recording/screen/status/updated".to_string(),
                ]),
            }),
        ),
    )
    .await??;

    let _running = wait_for_status(&mut mcp, ScreenRecordingState::Running).await?;

    let request_id = mcp.send_screen_recording_pause_request().await?;
    let _: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;

    let notification = timeout(
        std::time::Duration::from_millis(500),
        mcp.read_stream_until_notification_message("recording/screen/status/updated"),
    )
    .await;
    assert!(
        notification.is_err(),
        "screen recording notification should be filtered by optOutNotificationMethods"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(screen_recording)]
async fn screen_recording_feature_flag_gates_runtime_but_preserves_opt_in() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_config(
        &codex_home,
        r#"
[recording.screen]
enabled = true
"#,
    )?;

    let mut mcp = McpProcess::new_with_env(
        codex_home.path(),
        &[("CODEX_SCREEN_RECORDING_FAKE", Some("1"))],
    )
    .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.initialize_with_capabilities(
            ClientInfo {
                name: "codex_vscode".to_string(),
                title: Some("Codex VS Code Extension".to_string()),
                version: "0.1.0".to_string(),
            },
            Some(InitializeCapabilities {
                experimental_api: true,
                opt_out_notification_methods: None,
            }),
        ),
    )
    .await??;

    let disabled = read_screen_recording_status(&mut mcp).await?;
    assert_eq!(disabled.state, ScreenRecordingState::Disabled);
    assert_eq!(
        disabled.last_error.as_deref(),
        Some("screen recording feature is disabled")
    );

    let pause_request_id = mcp.send_screen_recording_pause_request().await?;
    let pause_error = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(pause_request_id)),
    )
    .await??;
    assert_invalid_request(
        pause_error,
        "screen recording feature is disabled".to_string(),
    );

    let enable_request_id = mcp
        .send_config_value_write_request(ConfigValueWriteParams {
            key_path: "features.screen_recording".to_string(),
            value: json!(true),
            merge_strategy: MergeStrategy::Replace,
            file_path: None,
            expected_version: None,
        })
        .await?;
    let _: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(enable_request_id)),
    )
    .await??;

    let running = wait_for_status(&mut mcp, ScreenRecordingState::Running).await?;
    assert_eq!(running.captured_display_count, 1);
    assert!(running.storage_path.as_path().exists());

    let disable_request_id = mcp
        .send_config_value_write_request(ConfigValueWriteParams {
            key_path: "features.screen_recording".to_string(),
            value: json!(false),
            merge_strategy: MergeStrategy::Replace,
            file_path: None,
            expected_version: None,
        })
        .await?;
    let _: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(disable_request_id)),
    )
    .await??;

    let disabled_notification = read_status_notification(&mut mcp, |status| {
        status.state == ScreenRecordingState::Disabled
            && status.last_error.as_deref() == Some("screen recording feature is disabled")
    })
    .await?;
    assert_eq!(
        disabled_notification.status.state,
        ScreenRecordingState::Disabled
    );
    assert_eq!(
        disabled_notification.status.last_error.as_deref(),
        Some("screen recording feature is disabled")
    );
    assert!(!running.storage_path.as_path().exists());

    Ok(())
}

async fn wait_for_status(
    mcp: &mut McpProcess,
    expected_state: ScreenRecordingState,
) -> Result<ScreenRecordingStatus> {
    let deadline = Instant::now() + DEFAULT_READ_TIMEOUT;
    loop {
        let status = read_screen_recording_status(mcp).await?;
        if status.state == expected_state {
            return Ok(status);
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for screen recording state {expected_state:?}, last state {:?}",
            status.state
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn read_screen_recording_status(mcp: &mut McpProcess) -> Result<ScreenRecordingStatus> {
    let request_id = mcp.send_screen_recording_read_request().await?;
    let response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let read: ScreenRecordingReadResponse = to_response(response)?;
    Ok(read.status)
}

async fn read_status_notification<F>(
    mcp: &mut McpProcess,
    predicate: F,
) -> Result<ScreenRecordingStatusUpdatedNotification>
where
    F: Fn(&ScreenRecordingStatus) -> bool,
{
    let notification: JSONRPCNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_matching_notification(
            "matching screen recording status update",
            |notification| {
                if notification.method != "recording/screen/status/updated" {
                    return false;
                }
                let Some(params) = notification.params.as_ref() else {
                    return false;
                };
                let Ok(parsed) = serde_json::from_value::<ScreenRecordingStatusUpdatedNotification>(
                    params.clone(),
                ) else {
                    return false;
                };
                predicate(&parsed.status)
            },
        ),
    )
    .await??;
    let params = notification
        .params
        .ok_or_else(|| anyhow::anyhow!("missing recording/screen/status/updated params"))?;
    Ok(serde_json::from_value(params)?)
}

fn assert_invalid_request(error: JSONRPCError, message: String) {
    assert_eq!(error.error.code, -32600);
    assert_eq!(error.error.message, message);
    assert_eq!(error.error.data, None);
}

async fn wait_for_segment_artifacts(
    storage_root: &Path,
) -> Result<(PathBuf, PathBuf, serde_json::Value)> {
    let deadline = Instant::now() + DEFAULT_READ_TIMEOUT;
    loop {
        if let Some((segment_path, manifest_path, manifest)) = find_segment_artifacts(storage_root)?
        {
            let segment_len = std::fs::metadata(&segment_path)
                .with_context(|| format!("read metadata for {segment_path:?}"))?
                .len();
            let frame_count = manifest
                .get("frame_count")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            if segment_len > 0 && frame_count > 0 {
                return Ok((segment_path, manifest_path, manifest));
            }
        }

        assert!(
            Instant::now() < deadline,
            "timed out waiting for screen recording artifacts in {storage_root:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn find_segment_artifacts(
    storage_root: &Path,
) -> Result<Option<(PathBuf, PathBuf, serde_json::Value)>> {
    let mut paths = std::fs::read_dir(storage_root)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "mp4"))
        .collect::<Vec<_>>();
    paths.sort();

    for segment_path in paths {
        let manifest_path = segment_path.with_extension("mp4.json");
        if !manifest_path.exists() {
            continue;
        }
        let manifest_bytes = std::fs::read(&manifest_path)
            .with_context(|| format!("read manifest {manifest_path:?}"))?;
        let manifest = serde_json::from_slice(&manifest_bytes)
            .with_context(|| format!("parse manifest {manifest_path:?}"))?;
        return Ok(Some((segment_path, manifest_path, manifest)));
    }

    Ok(None)
}

use super::backend::CaptureBackend;
use super::backend::CaptureBackendFailure;
use super::backend::CaptureBackendFailureKind;
use super::backend::default_capture_backend;
use super::ocr::OcrBackend;
use super::ocr::default_ocr_backend;
use super::persistence::CAPTURE_FPS;
use super::persistence::CaptureState;
use super::persistence::RETENTION_HOURS;
use super::persistence::capture_tick;
use super::persistence::prune_old_segments;
use super::persistence::purge_storage;
use crate::outgoing_message::OutgoingMessageSender;
use codex_app_server_protocol::ScreenRecordingPauseResponse;
use codex_app_server_protocol::ScreenRecordingPermission;
use codex_app_server_protocol::ScreenRecordingReadResponse;
use codex_app_server_protocol::ScreenRecordingResumeResponse;
use codex_app_server_protocol::ScreenRecordingState;
use codex_app_server_protocol::ScreenRecordingStatus;
use codex_app_server_protocol::ScreenRecordingStatusUpdatedNotification;
use codex_app_server_protocol::ServerNotification;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::fs::File;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

pub(crate) const SCREEN_RECORDING_FEATURE_DISABLED_MESSAGE: &str =
    "screen recording feature is disabled";
const SCREEN_RECORDING_OWNED_BY_ANOTHER_PROCESS_MESSAGE: &str =
    "screen recording is owned by another app-server process";
const LOCK_RETRY_INTERVAL: Duration = Duration::from_secs(5);

pub(crate) struct ScreenRecordingManager {
    inner: Arc<Inner>,
}

struct Inner {
    backend: Arc<dyn CaptureBackend>,
    ocr_backend: Arc<dyn OcrBackend>,
    outgoing: Arc<OutgoingMessageSender>,
    storage_root: PathBuf,
    lock_path: PathBuf,
    runtime: Mutex<RuntimeState>,
    capture_state: Arc<std::sync::Mutex<CaptureState>>,
    wake: Notify,
}

struct RuntimeState {
    feature_enabled: bool,
    config_enabled: bool,
    paused: bool,
    status: ScreenRecordingStatus,
    task: Option<JoinHandle<()>>,
    lock_retry: Option<JoinHandle<()>>,
    lock_retry_cancel: Option<CancellationToken>,
    cancel: Option<CancellationToken>,
    owner_lock: Option<File>,
    last_pruned_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusFingerprint {
    state: ScreenRecordingState,
    paused: bool,
    permission: ScreenRecordingPermission,
    captured_display_count: u32,
    last_error: Option<String>,
}

impl ScreenRecordingManager {
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        codex_home: &Path,
        feature_enabled: bool,
        config_enabled: bool,
    ) -> Self {
        Self::new_with_backend(
            outgoing,
            codex_home,
            feature_enabled,
            config_enabled,
            default_capture_backend(),
        )
    }

    pub(crate) fn new_with_backend(
        outgoing: Arc<OutgoingMessageSender>,
        codex_home: &Path,
        feature_enabled: bool,
        config_enabled: bool,
        backend: Arc<dyn CaptureBackend>,
    ) -> Self {
        let storage_root = codex_home.join("recording").join("screen_ephemeral");
        let lock_path = codex_home.join("recording").join("screen_ephemeral.lock");
        let storage_path = match AbsolutePathBuf::try_from(storage_root.clone()) {
            Ok(storage_path) => storage_path,
            Err(error) => {
                panic!("screen recording storage path should be absolute: {error}");
            }
        };
        let status = status_for_availability(
            feature_enabled,
            config_enabled,
            backend.platform(),
            backend.kind(),
            storage_path,
        );
        let manager = Self {
            inner: Arc::new(Inner {
                backend,
                ocr_backend: default_ocr_backend(),
                outgoing,
                storage_root,
                lock_path,
                runtime: Mutex::new(RuntimeState {
                    feature_enabled,
                    config_enabled,
                    paused: false,
                    status,
                    task: None,
                    lock_retry: None,
                    lock_retry_cancel: None,
                    cancel: None,
                    owner_lock: None,
                    last_pruned_at: None,
                }),
                capture_state: Arc::new(std::sync::Mutex::new(CaptureState::default())),
                wake: Notify::new(),
            }),
        };
        if feature_enabled && config_enabled {
            let manager_clone = manager.clone();
            tokio::spawn(async move {
                let _status = manager_clone
                    .reconcile(feature_enabled, config_enabled)
                    .await;
            });
        }
        manager
    }

    pub(crate) async fn feature_enabled(&self) -> bool {
        self.inner.runtime.lock().await.feature_enabled
    }

    pub(crate) async fn read(&self) -> ScreenRecordingReadResponse {
        ScreenRecordingReadResponse {
            status: self.current_status().await,
        }
    }

    pub(crate) async fn pause(&self) -> ScreenRecordingPauseResponse {
        let status = {
            let mut runtime = self.inner.runtime.lock().await;
            if !runtime.feature_enabled || !runtime.config_enabled {
                runtime.status.clone()
            } else {
                runtime.paused = true;
                let mut status = runtime.status.clone();
                status.paused = true;
                status.state = ScreenRecordingState::Paused;
                status
            }
        };
        self.inner.wake.notify_waiters();
        self.replace_status(status, /*notify*/ true).await;
        ScreenRecordingPauseResponse {
            status: self.current_status().await,
        }
    }

    pub(crate) async fn resume(&self) -> ScreenRecordingResumeResponse {
        let mut should_start_task = false;
        let status = {
            let mut runtime = self.inner.runtime.lock().await;
            if !runtime.feature_enabled || !runtime.config_enabled {
                runtime.status.clone()
            } else {
                runtime.paused = false;
                let mut status = runtime.status.clone();
                status.paused = false;
                status.state = ScreenRecordingState::Starting;
                if runtime.task.is_none() {
                    should_start_task = true;
                }
                status
            }
        };
        if should_start_task {
            self.ensure_task_running().await;
        } else {
            self.inner.wake.notify_waiters();
        }
        self.replace_status(status, /*notify*/ true).await;
        ScreenRecordingResumeResponse {
            status: self.current_status().await,
        }
    }

    pub(crate) async fn reconcile(
        &self,
        feature_enabled: bool,
        config_enabled: bool,
    ) -> ScreenRecordingStatus {
        if feature_enabled && config_enabled {
            self.ensure_task_running().await;
            let status = {
                let mut runtime = self.inner.runtime.lock().await;
                runtime.feature_enabled = true;
                runtime.config_enabled = true;
                let mut status = runtime.status.clone();
                status.paused = runtime.paused;
                if runtime.task.is_none() {
                    status.state = ScreenRecordingState::Error;
                    if !status.last_error.as_deref().is_some_and(|last_error| {
                        last_error.starts_with(SCREEN_RECORDING_OWNED_BY_ANOTHER_PROCESS_MESSAGE)
                    }) {
                        status.last_error =
                            Some(SCREEN_RECORDING_OWNED_BY_ANOTHER_PROCESS_MESSAGE.to_string());
                    }
                    status.captured_display_count = 0;
                } else if runtime.paused {
                    status.state = ScreenRecordingState::Paused;
                } else {
                    status.state = ScreenRecordingState::Starting;
                }
                status
            };
            self.replace_status(status, /*notify*/ true).await;
            return self.current_status().await;
        }

        let (storage_path, platform, backend_kind) = {
            let mut runtime = self.inner.runtime.lock().await;
            runtime.feature_enabled = feature_enabled;
            runtime.config_enabled = config_enabled;
            runtime.paused = false;
            (
                runtime.status.storage_path.clone(),
                runtime.status.platform,
                runtime.status.backend,
            )
        };
        self.stop_capture_and_reset().await;

        let status = {
            let mut runtime = self.inner.runtime.lock().await;
            runtime.last_pruned_at = None;
            status_for_availability(
                feature_enabled,
                config_enabled,
                platform,
                backend_kind,
                storage_path,
            )
        };
        self.replace_status(status, /*notify*/ true).await;
        self.current_status().await
    }

    pub(crate) async fn shutdown(&self) {
        self.stop_capture_and_reset().await;
    }

    async fn ensure_task_running(&self) {
        {
            let mut runtime = self.inner.runtime.lock().await;
            if !self.try_start_capture_locked(&mut runtime) {
                return;
            }
            spawn_capture_task(&self.inner, &mut runtime);
        }
        self.inner.wake.notify_waiters();
    }

    fn try_start_capture_locked(&self, runtime: &mut RuntimeState) -> bool {
        if runtime.task.is_some() {
            return false;
        }
        if runtime.owner_lock.is_some() {
            return true;
        }
        if let Some(parent) = self.inner.lock_path.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            let mut status = runtime.status.clone();
            status.state = ScreenRecordingState::Error;
            status.last_error = Some(format!(
                "failed to create screen recording lock directory: {err}"
            ));
            runtime.status = status;
            return false;
        }
        let lock_file = match File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.inner.lock_path)
        {
            Ok(lock_file) => lock_file,
            Err(err) => {
                let mut status = runtime.status.clone();
                status.state = ScreenRecordingState::Error;
                status.last_error =
                    Some(format!("failed to open screen recording lock file: {err}"));
                runtime.status = status;
                return false;
            }
        };
        match lock_file.try_lock() {
            Ok(()) => {
                if let Err(err) = write_owner_pid(&lock_file) {
                    let mut status = runtime.status.clone();
                    status.state = ScreenRecordingState::Error;
                    status.last_error =
                        Some(format!("failed to write screen recording owner pid: {err}"));
                    runtime.status = status;
                    return false;
                }
                runtime.owner_lock = Some(lock_file);
                runtime.status.state = if runtime.paused {
                    ScreenRecordingState::Paused
                } else {
                    ScreenRecordingState::Starting
                };
                runtime.status.paused = runtime.paused;
                runtime.status.last_error = None;
                true
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                let owner_pid = read_owner_pid(&self.inner.lock_path).ok();
                let mut status = runtime.status.clone();
                status.state = ScreenRecordingState::Error;
                status.last_error = Some(match owner_pid {
                    Some(pid) => {
                        format!("{SCREEN_RECORDING_OWNED_BY_ANOTHER_PROCESS_MESSAGE} (pid {pid})")
                    }
                    None => SCREEN_RECORDING_OWNED_BY_ANOTHER_PROCESS_MESSAGE.to_string(),
                });
                status.captured_display_count = 0;
                runtime.status = status;
                if runtime.lock_retry.is_none() {
                    let manager = ScreenRecordingManager {
                        inner: Arc::clone(&self.inner),
                    };
                    let cancel = CancellationToken::new();
                    let cancel_for_task = cancel.clone();
                    runtime.lock_retry_cancel = Some(cancel);
                    runtime.lock_retry = Some(tokio::spawn(async move {
                        loop {
                            tokio::select! {
                                _ = cancel_for_task.cancelled() => break,
                                _ = tokio::time::sleep(LOCK_RETRY_INTERVAL) => {}
                            }

                            let should_continue = {
                                let mut runtime = manager.inner.runtime.lock().await;
                                if !runtime.feature_enabled
                                    || !runtime.config_enabled
                                    || runtime.task.is_some()
                                {
                                    runtime.lock_retry = None;
                                    runtime.lock_retry_cancel = None;
                                    false
                                } else {
                                    let started = manager.try_start_capture_locked(&mut runtime);
                                    if started {
                                        spawn_capture_task(&manager.inner, &mut runtime);
                                        runtime.lock_retry = None;
                                        runtime.lock_retry_cancel = None;
                                        manager.inner.wake.notify_waiters();
                                        false
                                    } else {
                                        true
                                    }
                                }
                            };
                            if !should_continue {
                                break;
                            }
                        }
                    }));
                }
                false
            }
            Err(err) => {
                let mut status = runtime.status.clone();
                status.state = ScreenRecordingState::Error;
                status.last_error =
                    Some(format!("failed to lock screen recording owner file: {err}"));
                runtime.status = status;
                false
            }
        }
    }

    async fn stop_capture_and_reset(&self) {
        let (cancel, task, lock_retry_cancel, lock_retry) = {
            let mut runtime = self.inner.runtime.lock().await;
            (
                runtime.cancel.take(),
                runtime.task.take(),
                runtime.lock_retry_cancel.take(),
                runtime.lock_retry.take(),
            )
        };
        if let Some(lock_retry_cancel) = lock_retry_cancel {
            lock_retry_cancel.cancel();
        }
        if let Some(lock_retry) = lock_retry {
            let _ = lock_retry.await;
        }
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
        if let Some(task) = task {
            let _ = task.await;
        }
        let owner_lock = {
            let mut runtime = self.inner.runtime.lock().await;
            runtime.owner_lock.take()
        };
        if owner_lock.is_some() {
            let _ = purge_storage(&self.inner.storage_root);
            if let Ok(mut capture_state) = self.inner.capture_state.lock() {
                *capture_state = CaptureState::default();
            }
        }
        if owner_lock.is_some() {
            drop(owner_lock);
            if let Err(err) = std::fs::remove_file(&self.inner.lock_path)
                && err.kind() != ErrorKind::NotFound
            {
                tracing::debug!(
                    "failed to remove screen recording lock file {}: {err}",
                    self.inner.lock_path.display()
                );
            }
        }
    }

    async fn current_status(&self) -> ScreenRecordingStatus {
        self.inner.runtime.lock().await.status.clone()
    }

    async fn replace_status(&self, status: ScreenRecordingStatus, notify: bool) {
        let should_notify = {
            let mut runtime = self.inner.runtime.lock().await;
            let old_fingerprint = fingerprint(&runtime.status);
            let new_fingerprint = fingerprint(&status);
            runtime.status = status.clone();
            notify && old_fingerprint != new_fingerprint
        };
        if should_notify {
            self.inner
                .outgoing
                .send_server_notification(ServerNotification::ScreenRecordingStatusUpdated(
                    ScreenRecordingStatusUpdatedNotification { status },
                ))
                .await;
        }
    }
}

impl Clone for ScreenRecordingManager {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Inner {
    async fn run(self: Arc<Self>, cancel: CancellationToken) {
        let now = chrono::Utc::now();
        self.prune_if_needed(now).await;

        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {}
                _ = self.wake.notified() => {}
            }

            if cancel.is_cancelled() {
                break;
            }

            let paused = {
                let runtime = self.runtime.lock().await;
                !runtime.feature_enabled || !runtime.config_enabled || runtime.paused
            };
            if paused {
                continue;
            }

            let backend = Arc::clone(&self.backend);
            let ocr_backend = Arc::clone(&self.ocr_backend);
            let capture_state = Arc::clone(&self.capture_state);
            let storage_root = self.storage_root.clone();
            let captured_at = chrono::Utc::now();

            let result = tokio::task::spawn_blocking(move || {
                let mut capture_state = match capture_state.lock() {
                    Ok(capture_state) => capture_state,
                    Err(_) => {
                        return Err(CaptureBackendFailure::other(
                            "screen recording capture state lock should not be poisoned",
                        ));
                    }
                };
                capture_tick(
                    &storage_root,
                    &mut capture_state,
                    backend.as_ref(),
                    ocr_backend.as_ref(),
                    captured_at,
                )
            })
            .await;

            match result {
                Ok(Ok(outcome)) => {
                    self.prune_if_needed(captured_at).await;
                    let status = {
                        let runtime = self.runtime.lock().await;
                        let mut status = runtime.status.clone();
                        status.state = ScreenRecordingState::Running;
                        status.paused = false;
                        status.permission = ScreenRecordingPermission::Granted;
                        status.captured_display_count = outcome.captured_display_count;
                        status.newest_frame_at = outcome.newest_frame_at.or(status.newest_frame_at);
                        status.last_error = None;
                        status
                    };
                    ScreenRecordingManager {
                        inner: Arc::clone(&self),
                    }
                    .replace_status(status, /*notify*/ true)
                    .await;
                }
                Ok(Err(error)) => {
                    let status = {
                        let runtime = self.runtime.lock().await;
                        let mut status = runtime.status.clone();
                        apply_backend_error_status(&mut status, &error);
                        status
                    };
                    ScreenRecordingManager {
                        inner: Arc::clone(&self),
                    }
                    .replace_status(status, /*notify*/ true)
                    .await;
                }
                Err(join_error) => {
                    let status = {
                        let runtime = self.runtime.lock().await;
                        let mut status = runtime.status.clone();
                        status.state = ScreenRecordingState::Error;
                        status.last_error =
                            Some(format!("screen recording worker failed: {join_error}"));
                        status
                    };
                    ScreenRecordingManager {
                        inner: Arc::clone(&self),
                    }
                    .replace_status(status, /*notify*/ true)
                    .await;
                }
            }
        }
    }

    async fn prune_if_needed(&self, captured_at: chrono::DateTime<chrono::Utc>) {
        let should_prune = {
            let mut runtime = self.runtime.lock().await;
            match runtime.last_pruned_at {
                Some(last_pruned_at) if captured_at.timestamp() - last_pruned_at < 60 => false,
                _ => {
                    runtime.last_pruned_at = Some(captured_at.timestamp());
                    true
                }
            }
        };
        if !should_prune {
            return;
        }

        let storage_root = self.storage_root.clone();
        let _ = tokio::task::spawn_blocking(move || prune_old_segments(&storage_root, captured_at))
            .await;
    }
}

fn fingerprint(status: &ScreenRecordingStatus) -> StatusFingerprint {
    StatusFingerprint {
        state: status.state,
        paused: status.paused,
        permission: status.permission,
        captured_display_count: status.captured_display_count,
        last_error: status.last_error.clone(),
    }
}

fn spawn_capture_task(inner: &Arc<Inner>, runtime: &mut RuntimeState) {
    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();
    let inner = Arc::clone(inner);
    runtime.cancel = Some(cancel);
    runtime.task = Some(tokio::spawn(async move {
        inner.run(cancel_for_task).await;
    }));
}

fn write_owner_pid(mut lock_file: &File) -> std::io::Result<()> {
    lock_file.set_len(0)?;
    lock_file.seek(SeekFrom::Start(0))?;
    writeln!(lock_file, "{}", std::process::id())?;
    lock_file.sync_all()
}

fn read_owner_pid(lock_path: &Path) -> std::io::Result<u32> {
    let mut last_err = None;
    for _attempt in 0..5 {
        let mut lock_file = match File::options().read(true).open(lock_path) {
            Ok(lock_file) => lock_file,
            Err(err) => {
                last_err = Some(err);
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
        };
        lock_file.seek(SeekFrom::Start(0))?;
        let mut contents = String::new();
        if let Err(err) = lock_file.read_to_string(&mut contents) {
            last_err = Some(err);
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        match contents.trim().parse::<u32>() {
            Ok(pid) => return Ok(pid),
            Err(err) => {
                last_err = Some(std::io::Error::new(ErrorKind::InvalidData, err));
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    match last_err {
        Some(err) => Err(std::io::Error::new(ErrorKind::InvalidData, err)),
        None => Err(std::io::Error::new(
            ErrorKind::UnexpectedEof,
            "screen recording lock file was empty",
        )),
    }
}

fn status_for_availability(
    feature_enabled: bool,
    config_enabled: bool,
    platform: codex_app_server_protocol::ScreenRecordingPlatform,
    backend: codex_app_server_protocol::ScreenRecordingBackend,
    storage_path: AbsolutePathBuf,
) -> ScreenRecordingStatus {
    ScreenRecordingStatus {
        state: if feature_enabled && config_enabled {
            ScreenRecordingState::Starting
        } else {
            ScreenRecordingState::Disabled
        },
        paused: false,
        platform,
        backend,
        permission: ScreenRecordingPermission::Unknown,
        capture_fps: CAPTURE_FPS,
        retention_hours: RETENTION_HOURS,
        storage_path,
        captured_display_count: 0,
        newest_frame_at: None,
        last_error: (!feature_enabled && config_enabled)
            .then(|| SCREEN_RECORDING_FEATURE_DISABLED_MESSAGE.to_string()),
    }
}

fn apply_backend_error_status(status: &mut ScreenRecordingStatus, error: &CaptureBackendFailure) {
    status.captured_display_count = 0;
    status.last_error = Some(error.message.clone());
    status.permission = error.permission;
    status.state = match error.kind {
        CaptureBackendFailureKind::Unsupported => ScreenRecordingState::Unsupported,
        CaptureBackendFailureKind::PermissionRequired | CaptureBackendFailureKind::Other => {
            ScreenRecordingState::Error
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outgoing_message::OutgoingEnvelope;
    use crate::recording::backend::CaptureBackend;
    use crate::recording::backend::CaptureBackendFailure;
    use crate::recording::backend::CapturedDisplay;
    use crate::recording::backend::DisplayGeometry;
    use codex_app_server_protocol::ScreenRecordingBackend;
    use codex_app_server_protocol::ScreenRecordingPermission;
    use codex_app_server_protocol::ScreenRecordingPlatform;
    use image::Rgba;
    use image::RgbaImage;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use tokio::time::Instant;

    struct StaticBackend {
        result: Result<Vec<CapturedDisplay>, CaptureBackendFailure>,
    }

    impl CaptureBackend for StaticBackend {
        fn kind(&self) -> ScreenRecordingBackend {
            ScreenRecordingBackend::Xcap
        }

        fn platform(&self) -> ScreenRecordingPlatform {
            ScreenRecordingPlatform::Macos
        }

        fn capture_displays(&self) -> Result<Vec<CapturedDisplay>, CaptureBackendFailure> {
            self.result.clone()
        }
    }

    fn successful_backend() -> Arc<dyn CaptureBackend> {
        let mut frame = RgbaImage::new(8, 8);
        for pixel in frame.pixels_mut() {
            *pixel = Rgba([40, 80, 120, 255]);
        }
        Arc::new(StaticBackend {
            result: Ok(vec![CapturedDisplay {
                id: "display-1".to_string(),
                name: "Display 1".to_string(),
                geometry: DisplayGeometry {
                    width: 8,
                    height: 8,
                    rotation_millidegrees: 0,
                    scale_factor_milli: 1000,
                },
                frame,
            }]),
        })
    }

    async fn wait_for_state(
        manager: &ScreenRecordingManager,
        expected_state: ScreenRecordingState,
    ) -> ScreenRecordingStatus {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let status = manager.current_status().await;
            if status.state == expected_state {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for state {expected_state:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    #[test]
    fn read_owner_pid_retries_until_pid_is_written() {
        let temp_dir = TempDir::new().expect("tmpdir");
        let lock_path = temp_dir.path().join("recording.lock");
        File::create(&lock_path).expect("create lock file");
        let expected_pid = 4242_u32;
        let writer_path = lock_path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            std::fs::write(&writer_path, format!("{expected_pid}\n")).expect("write owner pid");
        });

        let pid = read_owner_pid(&lock_path).expect("read owner pid");

        writer.join().expect("join writer");
        assert_eq!(pid, expected_pid);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enabled_manager_autostarts_capture() {
        let temp_dir = TempDir::new().expect("tmpdir");
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(tx));
        let manager = ScreenRecordingManager::new_with_backend(
            outgoing,
            temp_dir.path(),
            true,
            true,
            successful_backend(),
        );

        let status = wait_for_state(&manager, ScreenRecordingState::Running).await;
        assert_eq!(status.permission, ScreenRecordingPermission::Granted);
        assert_eq!(status.captured_display_count, 1);
        manager.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_and_resume_change_runtime_state() {
        let temp_dir = TempDir::new().expect("tmpdir");
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(tx));
        let manager = ScreenRecordingManager::new_with_backend(
            outgoing,
            temp_dir.path(),
            true,
            true,
            successful_backend(),
        );
        let _running = wait_for_state(&manager, ScreenRecordingState::Running).await;

        let paused = manager.pause().await.status;
        assert_eq!(paused.state, ScreenRecordingState::Paused);
        assert!(paused.paused);

        let resumed = manager.resume().await.status;
        assert_eq!(resumed.state, ScreenRecordingState::Starting);
        let running = wait_for_state(&manager, ScreenRecordingState::Running).await;
        assert!(!running.paused);
        manager.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disabling_purges_storage() {
        let temp_dir = TempDir::new().expect("tmpdir");
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(tx));
        let manager = ScreenRecordingManager::new_with_backend(
            outgoing,
            temp_dir.path(),
            true,
            true,
            successful_backend(),
        );
        let running = wait_for_state(&manager, ScreenRecordingState::Running).await;
        assert!(running.storage_path.as_path().exists());

        let disabled = manager.reconcile(true, false).await;
        assert_eq!(disabled.state, ScreenRecordingState::Disabled);
        assert!(!running.storage_path.as_path().exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn feature_gate_blocks_start_until_enabled() {
        let temp_dir = TempDir::new().expect("tmpdir");
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(tx));
        let manager = ScreenRecordingManager::new_with_backend(
            outgoing,
            temp_dir.path(),
            false,
            true,
            successful_backend(),
        );

        let disabled = manager.current_status().await;
        assert_eq!(disabled.state, ScreenRecordingState::Disabled);
        assert_eq!(
            disabled.last_error.as_deref(),
            Some(SCREEN_RECORDING_FEATURE_DISABLED_MESSAGE)
        );

        let running = manager
            .reconcile(/*feature_enabled*/ true, /*config_enabled*/ true)
            .await;
        assert_eq!(running.state, ScreenRecordingState::Starting);
        let running = wait_for_state(&manager, ScreenRecordingState::Running).await;
        assert_eq!(running.captured_display_count, 1);
        manager.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_manager_shares_codex_home_but_does_not_start_capture() {
        let temp_dir = TempDir::new().expect("tmpdir");
        let (tx_a, _rx_a) = mpsc::channel::<OutgoingEnvelope>(8);
        let (tx_b, _rx_b) = mpsc::channel::<OutgoingEnvelope>(8);
        let manager_a = ScreenRecordingManager::new_with_backend(
            Arc::new(OutgoingMessageSender::new(tx_a)),
            temp_dir.path(),
            true,
            true,
            successful_backend(),
        );
        let manager_b = ScreenRecordingManager::new_with_backend(
            Arc::new(OutgoingMessageSender::new(tx_b)),
            temp_dir.path(),
            true,
            true,
            successful_backend(),
        );

        let deadline = Instant::now() + Duration::from_secs(3);
        let (owner, non_owner, owner_status, non_owner_status) = loop {
            let status_a = manager_a.current_status().await;
            let status_b = manager_b.current_status().await;
            match (status_a.state, status_b.state) {
                (ScreenRecordingState::Running, ScreenRecordingState::Error) => {
                    break (&manager_a, &manager_b, status_a, status_b);
                }
                (ScreenRecordingState::Error, ScreenRecordingState::Running) => {
                    break (&manager_b, &manager_a, status_b, status_a);
                }
                _ => {}
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for one manager to run and one to report lock contention"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };

        assert_eq!(owner_status.captured_display_count, 1);
        let expected_non_owner_error = format!(
            "{SCREEN_RECORDING_OWNED_BY_ANOTHER_PROCESS_MESSAGE} (pid {})",
            std::process::id()
        );
        assert_eq!(
            non_owner_status.last_error.as_deref(),
            Some(expected_non_owner_error.as_str())
        );
        assert_eq!(non_owner_status.captured_display_count, 0);

        non_owner.shutdown().await;
        let owner_storage_path = owner_status.storage_path.clone();
        assert!(
            owner_storage_path.as_path().exists(),
            "non-owner shutdown should not purge owner storage"
        );

        let disabled = owner.reconcile(true, false).await;
        assert_eq!(disabled.state, ScreenRecordingState::Disabled);
        assert!(!owner_storage_path.as_path().exists());
        owner.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lock_failover_retries_and_only_one_successor_wins() {
        let temp_dir = TempDir::new().expect("tmpdir");
        let (tx_a, _rx_a) = mpsc::channel::<OutgoingEnvelope>(8);
        let (tx_b, _rx_b) = mpsc::channel::<OutgoingEnvelope>(8);
        let (tx_c, _rx_c) = mpsc::channel::<OutgoingEnvelope>(8);
        let manager_a = ScreenRecordingManager::new_with_backend(
            Arc::new(OutgoingMessageSender::new(tx_a)),
            temp_dir.path(),
            true,
            true,
            successful_backend(),
        );
        let manager_b = ScreenRecordingManager::new_with_backend(
            Arc::new(OutgoingMessageSender::new(tx_b)),
            temp_dir.path(),
            true,
            true,
            successful_backend(),
        );
        let manager_c = ScreenRecordingManager::new_with_backend(
            Arc::new(OutgoingMessageSender::new(tx_c)),
            temp_dir.path(),
            true,
            true,
            successful_backend(),
        );

        let initial_deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let running = [
                manager_a.current_status().await.state,
                manager_b.current_status().await.state,
                manager_c.current_status().await.state,
            ]
            .into_iter()
            .filter(|state| *state == ScreenRecordingState::Running)
            .count();
            if running == 1 {
                break;
            }
            assert!(
                Instant::now() < initial_deadline,
                "timed out waiting for initial owner"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let was_owner_a = manager_a.current_status().await.state == ScreenRecordingState::Running;
        let was_owner_b = manager_b.current_status().await.state == ScreenRecordingState::Running;
        let was_owner_c = manager_c.current_status().await.state == ScreenRecordingState::Running;
        if was_owner_a {
            manager_a.shutdown().await;
        } else if was_owner_b {
            manager_b.shutdown().await;
        } else {
            manager_c.shutdown().await;
        }

        let failover_deadline = Instant::now() + LOCK_RETRY_INTERVAL + Duration::from_secs(3);
        loop {
            let mut states = Vec::new();
            if !was_owner_a {
                states.push(manager_a.current_status().await.state);
            }
            if !was_owner_b {
                states.push(manager_b.current_status().await.state);
            }
            if !was_owner_c {
                states.push(manager_c.current_status().await.state);
            }
            let running = states
                .iter()
                .filter(|state| **state == ScreenRecordingState::Running)
                .count();
            if running == 1 {
                break;
            }
            assert!(
                Instant::now() < failover_deadline,
                "timed out waiting for exactly one successor to acquire lock, states={states:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        manager_a.shutdown().await;
        manager_b.shutdown().await;
        manager_c.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn same_process_contention_reports_own_pid() {
        let temp_dir = TempDir::new().expect("tmpdir");
        let (tx_a, _rx_a) = mpsc::channel::<OutgoingEnvelope>(8);
        let (tx_b, _rx_b) = mpsc::channel::<OutgoingEnvelope>(8);
        let manager_a = ScreenRecordingManager::new_with_backend(
            Arc::new(OutgoingMessageSender::new(tx_a)),
            temp_dir.path(),
            true,
            true,
            successful_backend(),
        );
        let manager_b = ScreenRecordingManager::new_with_backend(
            Arc::new(OutgoingMessageSender::new(tx_b)),
            temp_dir.path(),
            true,
            true,
            successful_backend(),
        );

        let deadline = Instant::now() + Duration::from_secs(3);
        let contended = loop {
            let status_a = manager_a.current_status().await;
            let status_b = manager_b.current_status().await;
            match (status_a.state, status_b.state) {
                (ScreenRecordingState::Running, ScreenRecordingState::Error) => break status_b,
                (ScreenRecordingState::Error, ScreenRecordingState::Running) => break status_a,
                _ => {}
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for same-process contention"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };

        assert_eq!(
            contended.last_error.as_deref(),
            Some(
                format!(
                    "{SCREEN_RECORDING_OWNED_BY_ANOTHER_PROCESS_MESSAGE} (pid {})",
                    std::process::id()
                )
                .as_str()
            )
        );

        manager_a.shutdown().await;
        manager_b.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_contender_does_not_reacquire_lock_after_owner_exits() {
        let temp_dir = TempDir::new().expect("tmpdir");
        let (tx_a, _rx_a) = mpsc::channel::<OutgoingEnvelope>(8);
        let owner = ScreenRecordingManager::new_with_backend(
            Arc::new(OutgoingMessageSender::new(tx_a)),
            temp_dir.path(),
            true,
            true,
            successful_backend(),
        );
        let _owner_running = wait_for_state(&owner, ScreenRecordingState::Running).await;

        let (tx_b, _rx_b) = mpsc::channel::<OutgoingEnvelope>(8);
        let transient = ScreenRecordingManager::new_with_backend(
            Arc::new(OutgoingMessageSender::new(tx_b)),
            temp_dir.path(),
            true,
            true,
            successful_backend(),
        );
        let transient_error = wait_for_state(&transient, ScreenRecordingState::Error).await;
        assert_eq!(
            transient_error.last_error.as_deref(),
            Some(
                format!(
                    "{SCREEN_RECORDING_OWNED_BY_ANOTHER_PROCESS_MESSAGE} (pid {})",
                    std::process::id()
                )
                .as_str()
            )
        );

        tokio::time::sleep(Duration::from_millis(200)).await;
        transient.shutdown().await;

        let (tx_c, _rx_c) = mpsc::channel::<OutgoingEnvelope>(8);
        let main = ScreenRecordingManager::new_with_backend(
            Arc::new(OutgoingMessageSender::new(tx_c)),
            temp_dir.path(),
            true,
            true,
            successful_backend(),
        );
        let _main_error = wait_for_state(&main, ScreenRecordingState::Error).await;

        owner.shutdown().await;

        let failover_deadline = Instant::now() + LOCK_RETRY_INTERVAL + Duration::from_secs(3);
        loop {
            let main_status = main.current_status().await;
            if main_status.state == ScreenRecordingState::Running {
                break;
            }
            let transient_status = transient.current_status().await;
            assert_ne!(
                transient_status.state,
                ScreenRecordingState::Running,
                "shut down transient manager should not reacquire the recording lock"
            );
            assert!(
                Instant::now() < failover_deadline,
                "timed out waiting for main manager to acquire lock after owner exit, main={main_status:?}, transient={transient_status:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        main.shutdown().await;
    }
}

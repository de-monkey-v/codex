use codex_app_server_protocol::ScreenRecordingBackend;
use codex_app_server_protocol::ScreenRecordingPermission;
use codex_app_server_protocol::ScreenRecordingPlatform;
use image::Rgba;
use image::RgbaImage;
use std::sync::Arc;
#[cfg(target_os = "macos")]
use xcap::Monitor;
#[cfg(target_os = "macos")]
use xcap::XCapError;

pub(crate) const FAKE_BACKEND_ENV_VAR: &str = "CODEX_SCREEN_RECORDING_FAKE";

pub(crate) trait CaptureBackend: Send + Sync {
    fn kind(&self) -> ScreenRecordingBackend;
    fn platform(&self) -> ScreenRecordingPlatform;
    fn capture_displays(&self) -> Result<Vec<CapturedDisplay>, CaptureBackendFailure>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapturedDisplay {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) geometry: DisplayGeometry,
    pub(crate) frame: RgbaImage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DisplayGeometry {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rotation_millidegrees: i32,
    pub(crate) scale_factor_milli: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureBackendFailureKind {
    Unsupported,
    PermissionRequired,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CaptureBackendFailure {
    pub(crate) kind: CaptureBackendFailureKind,
    pub(crate) permission: ScreenRecordingPermission,
    pub(crate) message: String,
}

impl CaptureBackendFailure {
    pub(crate) fn unsupported(message: impl Into<String>) -> Self {
        Self {
            kind: CaptureBackendFailureKind::Unsupported,
            permission: ScreenRecordingPermission::Unknown,
            message: message.into(),
        }
    }

    pub(crate) fn permission_required(message: impl Into<String>) -> Self {
        Self {
            kind: CaptureBackendFailureKind::PermissionRequired,
            permission: ScreenRecordingPermission::Required,
            message: message.into(),
        }
    }

    pub(crate) fn other(message: impl Into<String>) -> Self {
        Self {
            kind: CaptureBackendFailureKind::Other,
            permission: ScreenRecordingPermission::Unknown,
            message: message.into(),
        }
    }
}

pub(crate) fn default_capture_backend() -> Arc<dyn CaptureBackend> {
    if std::env::var_os(FAKE_BACKEND_ENV_VAR).is_some() {
        Arc::new(FakeCaptureBackend)
    } else {
        platform_capture_backend()
    }
}

#[cfg(target_os = "macos")]
fn platform_capture_backend() -> Arc<dyn CaptureBackend> {
    Arc::new(XcapCaptureBackend)
}

#[cfg(not(target_os = "macos"))]
fn platform_capture_backend() -> Arc<dyn CaptureBackend> {
    Arc::new(UnsupportedCaptureBackend)
}

#[cfg(target_os = "macos")]
pub(crate) struct XcapCaptureBackend;

#[cfg(target_os = "macos")]
impl CaptureBackend for XcapCaptureBackend {
    fn kind(&self) -> ScreenRecordingBackend {
        ScreenRecordingBackend::Xcap
    }

    fn platform(&self) -> ScreenRecordingPlatform {
        current_platform()
    }

    fn capture_displays(&self) -> Result<Vec<CapturedDisplay>, CaptureBackendFailure> {
        let monitors = Monitor::all().map_err(map_xcap_error)?;
        let mut displays = Vec::with_capacity(monitors.len());
        for monitor in monitors {
            displays.push(CapturedDisplay {
                id: monitor
                    .id()
                    .map(|id| id.to_string())
                    .map_err(map_xcap_error)?,
                name: monitor
                    .friendly_name()
                    .or_else(|_| monitor.name())
                    .map_err(map_xcap_error)?,
                geometry: DisplayGeometry {
                    width: monitor.width().map_err(map_xcap_error)?,
                    height: monitor.height().map_err(map_xcap_error)?,
                    rotation_millidegrees: (monitor.rotation().map_err(map_xcap_error)? * 1000.0)
                        .round() as i32,
                    scale_factor_milli: (monitor.scale_factor().map_err(map_xcap_error)? * 1000.0)
                        .round() as u32,
                },
                frame: monitor.capture_image().map_err(map_xcap_error)?,
            });
        }
        Ok(displays)
    }
}

#[cfg(not(target_os = "macos"))]
struct UnsupportedCaptureBackend;

#[cfg(not(target_os = "macos"))]
impl CaptureBackend for UnsupportedCaptureBackend {
    fn kind(&self) -> ScreenRecordingBackend {
        ScreenRecordingBackend::Xcap
    }

    fn platform(&self) -> ScreenRecordingPlatform {
        current_platform()
    }

    fn capture_displays(&self) -> Result<Vec<CapturedDisplay>, CaptureBackendFailure> {
        Err(CaptureBackendFailure::unsupported(
            "screen capture is currently supported only on macOS",
        ))
    }
}

struct FakeCaptureBackend;

impl CaptureBackend for FakeCaptureBackend {
    fn kind(&self) -> ScreenRecordingBackend {
        ScreenRecordingBackend::Xcap
    }

    fn platform(&self) -> ScreenRecordingPlatform {
        current_platform()
    }

    fn capture_displays(&self) -> Result<Vec<CapturedDisplay>, CaptureBackendFailure> {
        let mut frame = RgbaImage::new(64, 48);
        for (x, y, pixel) in frame.enumerate_pixels_mut() {
            let red = ((x * 4) % 255) as u8;
            let green = ((y * 5) % 255) as u8;
            *pixel = Rgba([red, green, 180, 255]);
        }
        Ok(vec![CapturedDisplay {
            id: "fake-display-1".to_string(),
            name: "Fake Display".to_string(),
            geometry: DisplayGeometry {
                width: 64,
                height: 48,
                rotation_millidegrees: 0,
                scale_factor_milli: 1000,
            },
            frame,
        }])
    }
}

fn current_platform() -> ScreenRecordingPlatform {
    #[cfg(target_os = "macos")]
    {
        ScreenRecordingPlatform::Macos
    }
    #[cfg(target_os = "windows")]
    {
        ScreenRecordingPlatform::Windows
    }
    #[cfg(target_os = "linux")]
    {
        ScreenRecordingPlatform::Linux
    }
}

#[cfg(target_os = "macos")]
fn map_xcap_error(error: XCapError) -> CaptureBackendFailure {
    match error {
        XCapError::NotSupported => {
            CaptureBackendFailure::unsupported("screen capture is not supported on this host")
        }
        other => {
            let message = other.to_string();
            let normalized = message.to_ascii_lowercase();
            if normalized.contains("permission")
                || normalized.contains("denied")
                || normalized.contains("not authorized")
                || normalized.contains("not permitted")
                || normalized.contains("access")
            {
                CaptureBackendFailure::permission_required(message)
            } else {
                CaptureBackendFailure::other(message)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn fake_backend_returns_one_display() {
        let backend = FakeCaptureBackend;
        let displays = backend
            .capture_displays()
            .expect("fake backend should succeed");
        assert_eq!(displays.len(), 1);
        assert_eq!(displays[0].id, "fake-display-1");
        assert_eq!(displays[0].geometry.width, 64);
        assert_eq!(displays[0].geometry.height, 48);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn platform_backend_reports_macos_only_support() {
        let backend = platform_capture_backend();
        let err = backend
            .capture_displays()
            .expect_err("non-macOS backend should be unsupported");
        assert_eq!(err.kind, CaptureBackendFailureKind::Unsupported);
        assert_eq!(
            err.message,
            "screen capture is currently supported only on macOS"
        );
    }
}

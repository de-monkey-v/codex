use super::backend::CaptureBackend;
use super::backend::CaptureBackendFailure;
use super::backend::CapturedDisplay;
use super::backend::DisplayGeometry;
use super::encoder::FfmpegSegmentEncoder;
use super::ocr::OcrBackend;
use super::ocr::OcrFrameResult;
use super::ocr::OcrInput;
use chrono::DateTime;
use chrono::Utc;
use image::ColorType;
use image::ImageEncoder;
use image::codecs::jpeg::JpegEncoder;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use walkdir::WalkDir;

pub(crate) const CAPTURE_FPS: u32 = 1;
pub(crate) const RETENTION_HOURS: u32 = 6;
pub(crate) const RETENTION_SECONDS: i64 = 6 * 60 * 60;
pub(crate) const SEGMENT_LENGTH_SECONDS: i64 = 30 * 60;
pub(crate) const DISPLAY_REMOVAL_MISSED_TICKS: u32 = 3;
const OCR_INTERVAL_SECONDS: u64 = 1;
const OCR_FRAME_INTERVAL: u64 = CAPTURE_FPS as u64 * OCR_INTERVAL_SECONDS;
const MANIFEST_FILE_EXTENSION: &str = "mp4.json";
const OCR_FILE_EXTENSION: &str = "ocr.jsonl";

#[derive(Default)]
pub(crate) struct CaptureState {
    display_streams: HashMap<String, DisplayStream>,
    known_display_ids: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CaptureTickOutcome {
    pub(crate) captured_display_count: u32,
    pub(crate) newest_frame_at: Option<i64>,
}

struct DisplayStream {
    name: String,
    geometry: DisplayGeometry,
    missed_ticks: u32,
    segment: SegmentState,
}

struct SegmentState {
    manifest_path: PathBuf,
    _latest_frame_path: PathBuf,
    ocr_path: PathBuf,
    bucket_start_at: i64,
    segment_started_at: i64,
    frame_index: u64,
    last_ocr_text: Option<String>,
    encoder: FfmpegSegmentEncoder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SegmentReason {
    Started,
    Hotplugged,
    GeometryChanged,
    Rotated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SegmentManifest {
    version: u32,
    display_id: String,
    display_name: String,
    width: u32,
    height: u32,
    rotation_millidegrees: i32,
    scale_factor_milli: u32,
    segment_started_at: i64,
    started_reason: SegmentReason,
    frame_count: u64,
    newest_frame_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OcrFrameRecord {
    version: u32,
    display_id: String,
    segment_started_at: i64,
    captured_at: i64,
    frame_index: u64,
    full_text: String,
}

pub(crate) fn capture_tick(
    storage_root: &Path,
    state: &mut CaptureState,
    backend: &dyn CaptureBackend,
    ocr_backend: &dyn OcrBackend,
    captured_at: DateTime<Utc>,
) -> Result<CaptureTickOutcome, CaptureBackendFailure> {
    let displays = backend.capture_displays()?;
    fs::create_dir_all(storage_root)
        .map_err(|err| CaptureBackendFailure::other(err.to_string()))?;

    let mut newest_frame_at = None;
    let seen_ids: HashSet<String> = displays.iter().map(|display| display.id.clone()).collect();

    for display in displays {
        let stream = upsert_display_stream(storage_root, state, &display, captured_at)
            .map_err(|err| CaptureBackendFailure::other(err.to_string()))?;
        write_frame(storage_root, stream, &display, ocr_backend, captured_at)
            .map_err(|err| CaptureBackendFailure::other(err.to_string()))?;
        newest_frame_at = Some(
            newest_frame_at
                .unwrap_or(captured_at.timestamp())
                .max(captured_at.timestamp()),
        );
    }

    let missing_ids = state
        .display_streams
        .keys()
        .filter(|id| !seen_ids.contains(*id))
        .cloned()
        .collect::<Vec<_>>();
    for id in missing_ids {
        if let Some(stream) = state.display_streams.get_mut(&id) {
            stream.missed_ticks = stream.missed_ticks.saturating_add(1);
            if stream.missed_ticks >= DISPLAY_REMOVAL_MISSED_TICKS {
                state.display_streams.remove(&id);
            }
        }
    }

    Ok(CaptureTickOutcome {
        captured_display_count: state
            .display_streams
            .values()
            .filter(|stream| stream.missed_ticks == 0)
            .count() as u32,
        newest_frame_at,
    })
}

pub(crate) fn prune_old_segments(
    storage_root: &Path,
    captured_at: DateTime<Utc>,
) -> std::io::Result<()> {
    if !storage_root.exists() {
        return Ok(());
    }

    let cutoff = captured_at.timestamp() - RETENTION_SECONDS;
    let manifest_paths = WalkDir::new(storage_root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_file()
                && entry
                    .path()
                    .to_str()
                    .is_some_and(|path| path.ends_with(".mp4.json"))
        })
        .map(walkdir::DirEntry::into_path)
        .collect::<Vec<_>>();

    for manifest_path in manifest_paths {
        let Ok(bytes) = fs::read(&manifest_path) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_slice::<SegmentManifest>(&bytes) else {
            continue;
        };
        let newest_frame_at = manifest
            .newest_frame_at
            .unwrap_or(manifest.segment_started_at);
        if newest_frame_at < cutoff {
            if let Some(segment_path) = segment_path_for_manifest(&manifest_path) {
                let _ = fs::remove_file(segment_path);
                let _ = fs::remove_file(latest_frame_path_for_segment(&manifest_path));
                let _ = fs::remove_file(ocr_path_for_segment(&manifest_path));
            }
            let _ = fs::remove_file(&manifest_path);
        }
    }

    Ok(())
}

pub(crate) fn purge_storage(storage_root: &Path) -> std::io::Result<()> {
    if storage_root.exists() {
        fs::remove_dir_all(storage_root)?;
    }
    Ok(())
}

fn upsert_display_stream<'a>(
    storage_root: &Path,
    state: &'a mut CaptureState,
    display: &CapturedDisplay,
    captured_at: DateTime<Utc>,
) -> std::io::Result<&'a mut DisplayStream> {
    let bucket_start_at = bucket_start_at(captured_at);
    let is_known_display = state.known_display_ids.contains(&display.id);

    let rotate_reason = if let Some(stream) = state.display_streams.get(&display.id) {
        if stream.geometry != display.geometry {
            Some(SegmentReason::GeometryChanged)
        } else if stream.segment.bucket_start_at != bucket_start_at {
            Some(SegmentReason::Rotated)
        } else {
            None
        }
    } else if is_known_display {
        Some(SegmentReason::Hotplugged)
    } else {
        Some(SegmentReason::Started)
    };

    if let Some(stream) = state.display_streams.get_mut(&display.id) {
        stream.name = display.name.clone();
        stream.geometry = display.geometry;
        stream.missed_ticks = 0;
        if let Some(reason) = rotate_reason {
            stream.segment = open_segment(storage_root, display, captured_at, reason)?;
        }
    } else {
        state.known_display_ids.insert(display.id.clone());
        let segment = open_segment(
            storage_root,
            display,
            captured_at,
            rotate_reason.unwrap_or(SegmentReason::Started),
        )?;
        state.display_streams.insert(
            display.id.clone(),
            DisplayStream {
                name: display.name.clone(),
                geometry: display.geometry,
                missed_ticks: 0,
                segment,
            },
        );
    }

    match state.display_streams.get_mut(&display.id) {
        Some(stream) => Ok(stream),
        None => Err(std::io::Error::other(
            "display stream should exist after upsert",
        )),
    }
}

fn open_segment(
    storage_root: &Path,
    display: &CapturedDisplay,
    captured_at: DateTime<Utc>,
    reason: SegmentReason,
) -> std::io::Result<SegmentState> {
    let bucket_start_at = bucket_start_at(captured_at);
    let encoded_width = display.frame.width();
    let encoded_height = display.frame.height();
    let segment_name = format!(
        "{}-display-{}",
        captured_at.format("%Y-%m-%dT%H-%M-%SZ"),
        display.id
    );
    let segment_path = storage_root.join(format!("{segment_name}.mp4"));
    let manifest_path = segment_path.with_extension(MANIFEST_FILE_EXTENSION);
    let manifest = SegmentManifest {
        version: 1,
        display_id: display.id.clone(),
        display_name: display.name.clone(),
        width: encoded_width,
        height: encoded_height,
        rotation_millidegrees: display.geometry.rotation_millidegrees,
        scale_factor_milli: display.geometry.scale_factor_milli,
        segment_started_at: captured_at.timestamp(),
        started_reason: reason,
        frame_count: 0,
        newest_frame_at: None,
    };
    write_manifest(&manifest_path, &manifest)?;
    let encoder =
        FfmpegSegmentEncoder::open(&segment_path, encoded_width, encoded_height, CAPTURE_FPS)?;
    Ok(SegmentState {
        manifest_path,
        _latest_frame_path: storage_root.join(format!("{segment_name}-latest.jpg")),
        ocr_path: storage_root.join(format!("{segment_name}.{OCR_FILE_EXTENSION}")),
        bucket_start_at,
        segment_started_at: captured_at.timestamp(),
        frame_index: 0,
        last_ocr_text: None,
        encoder,
    })
}

fn write_frame(
    _storage_root: &Path,
    stream: &mut DisplayStream,
    display: &CapturedDisplay,
    ocr_backend: &dyn OcrBackend,
    captured_at: DateTime<Utc>,
) -> std::io::Result<()> {
    stream
        .segment
        .encoder
        .write_rgba_frame(display.frame.as_raw())?;
    // Temporarily disable latest-frame JPEG generation; it currently dominates
    // the recorder CPU profile on high-resolution displays.
    // write_latest_frame(&stream.segment.latest_frame_path, display)?;
    maybe_write_ocr_frame(stream, display, ocr_backend, captured_at)?;
    stream.segment.frame_index = stream.segment.frame_index.saturating_add(1);

    let manifest_bytes = fs::read(&stream.segment.manifest_path)?;
    let mut manifest: SegmentManifest =
        serde_json::from_slice(&manifest_bytes).map_err(std::io::Error::other)?;
    manifest.frame_count = stream.segment.frame_index;
    manifest.newest_frame_at = Some(captured_at.timestamp());
    write_manifest(&stream.segment.manifest_path, &manifest)
}

#[allow(dead_code)]
fn write_latest_frame(frame_path: &Path, display: &CapturedDisplay) -> std::io::Result<()> {
    let temp_path = frame_path.with_extension("jpg.tmp");
    let file = File::create(&temp_path)?;
    let mut writer = BufWriter::new(file);
    let rgb_frame = image::DynamicImage::ImageRgba8(display.frame.clone()).into_rgb8();
    let encoder = JpegEncoder::new_with_quality(&mut writer, 85);
    let result = encoder
        .write_image(
            rgb_frame.as_raw(),
            rgb_frame.width(),
            rgb_frame.height(),
            ColorType::Rgb8.into(),
        )
        .map_err(std::io::Error::other)
        .and_then(|()| {
            writer.flush()?;
            writer.get_ref().sync_all()
        })
        .and_then(|()| fs::rename(&temp_path, frame_path));
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn write_manifest(manifest_path: &Path, manifest: &SegmentManifest) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(manifest).map_err(std::io::Error::other)?;
    fs::write(manifest_path, bytes)
}

fn maybe_write_ocr_frame(
    stream: &mut DisplayStream,
    display: &CapturedDisplay,
    ocr_backend: &dyn OcrBackend,
    captured_at: DateTime<Utc>,
) -> std::io::Result<()> {
    if !stream
        .segment
        .frame_index
        .is_multiple_of(OCR_FRAME_INTERVAL)
    {
        return Ok(());
    }
    let Some(ocr_result) = ocr_backend.recognize(OcrInput {
        frame: &display.frame,
    })?
    else {
        return Ok(());
    };
    let normalized_text = normalize_ocr_text(&ocr_result);
    if normalized_text.is_empty()
        || stream
            .segment
            .last_ocr_text
            .as_ref()
            .is_some_and(|last_ocr_text| last_ocr_text == &normalized_text)
    {
        return Ok(());
    }

    let record = OcrFrameRecord {
        version: 1,
        display_id: display.id.clone(),
        segment_started_at: stream.segment.segment_started_at,
        captured_at: captured_at.timestamp(),
        frame_index: stream.segment.frame_index,
        full_text: normalized_text.clone(),
    };
    append_ocr_record(&stream.segment.ocr_path, &record)?;
    stream.segment.last_ocr_text = Some(normalized_text);
    Ok(())
}

fn normalize_ocr_text(result: &OcrFrameResult) -> String {
    result
        .full_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn append_ocr_record(ocr_path: &Path, record: &OcrFrameRecord) -> std::io::Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(ocr_path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer(&mut writer, record).map_err(std::io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer.get_ref().sync_all()
}

fn segment_path_for_manifest(manifest_path: &Path) -> Option<PathBuf> {
    let path = manifest_path.to_str()?;
    Some(PathBuf::from(path.strip_suffix(".json")?))
}

fn latest_frame_path_for_segment(manifest_path: &Path) -> PathBuf {
    let Some(segment_path) = segment_path_for_manifest(manifest_path) else {
        panic!("manifest path should always have a matching segment path");
    };
    let Some(segment_stem) = segment_path.file_stem().and_then(|name| name.to_str()) else {
        panic!("segment path should have a valid utf8 stem");
    };
    segment_path.with_file_name(format!("{segment_stem}-latest.jpg",))
}

fn ocr_path_for_segment(manifest_path: &Path) -> PathBuf {
    let Some(segment_path) = segment_path_for_manifest(manifest_path) else {
        panic!("manifest path should always have a matching segment path");
    };
    segment_path.with_extension(OCR_FILE_EXTENSION)
}

fn bucket_start_at(captured_at: DateTime<Utc>) -> i64 {
    let timestamp = captured_at.timestamp();
    timestamp - (timestamp % SEGMENT_LENGTH_SECONDS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::backend::CaptureBackendFailureKind;
    use crate::recording::ocr::NoopOcrBackend;
    use crate::recording::ocr::OcrBackend;
    use crate::recording::ocr::OcrFrameResult;
    use crate::recording::ocr::OcrInput;
    use image::Rgba;
    use image::RgbaImage;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    struct SequenceBackend {
        frames: std::sync::Mutex<Vec<Result<Vec<CapturedDisplay>, CaptureBackendFailure>>>,
    }

    impl SequenceBackend {
        fn new(frames: Vec<Result<Vec<CapturedDisplay>, CaptureBackendFailure>>) -> Self {
            Self {
                frames: std::sync::Mutex::new(frames),
            }
        }
    }

    impl CaptureBackend for SequenceBackend {
        fn kind(&self) -> codex_app_server_protocol::ScreenRecordingBackend {
            codex_app_server_protocol::ScreenRecordingBackend::Xcap
        }

        fn platform(&self) -> codex_app_server_protocol::ScreenRecordingPlatform {
            codex_app_server_protocol::ScreenRecordingPlatform::Macos
        }

        fn capture_displays(&self) -> Result<Vec<CapturedDisplay>, CaptureBackendFailure> {
            self.frames.lock().expect("sequence lock").remove(0)
        }
    }

    struct SequenceOcrBackend {
        results: std::sync::Mutex<Vec<std::io::Result<Option<OcrFrameResult>>>>,
    }

    impl SequenceOcrBackend {
        fn new(results: Vec<std::io::Result<Option<OcrFrameResult>>>) -> Self {
            Self {
                results: std::sync::Mutex::new(results),
            }
        }
    }

    impl OcrBackend for SequenceOcrBackend {
        fn recognize(&self, _input: OcrInput<'_>) -> std::io::Result<Option<OcrFrameResult>> {
            self.results.lock().expect("ocr lock").remove(0)
        }
    }

    fn display(id: &str, width: u32, height: u32) -> CapturedDisplay {
        let mut frame = RgbaImage::new(width, height);
        for pixel in frame.pixels_mut() {
            *pixel = Rgba([10, 20, 30, 255]);
        }
        CapturedDisplay {
            id: id.to_string(),
            name: format!("Display {id}"),
            geometry: DisplayGeometry {
                width,
                height,
                rotation_millidegrees: 0,
                scale_factor_milli: 1000,
            },
            frame,
        }
    }

    fn hidpi_display(
        id: &str,
        logical_width: u32,
        logical_height: u32,
        scale: u32,
    ) -> CapturedDisplay {
        let mut display = display(id, logical_width * scale, logical_height * scale);
        display.geometry.width = logical_width;
        display.geometry.height = logical_height;
        display.geometry.scale_factor_milli = scale * 1000;
        display
    }

    #[test]
    fn hotplug_misses_are_debounced_for_three_ticks() {
        let tmp = TempDir::new().expect("tmpdir");
        let backend = SequenceBackend::new(vec![
            Ok(vec![display("1", 64, 48)]),
            Ok(vec![]),
            Ok(vec![]),
            Ok(vec![display("1", 64, 48)]),
        ]);
        let mut state = CaptureState::default();

        for second in 0..4 {
            capture_tick(
                tmp.path(),
                &mut state,
                &backend,
                &NoopOcrBackend,
                DateTime::from_timestamp(1_700_000_000 + second, 0).expect("timestamp"),
            )
            .expect("capture tick");
        }

        assert_eq!(state.display_streams.len(), 1);
        assert_eq!(state.display_streams["1"].missed_ticks, 0);
    }

    #[test]
    fn geometry_change_rotates_segments() {
        let tmp = TempDir::new().expect("tmpdir");
        let backend = SequenceBackend::new(vec![
            Ok(vec![display("1", 64, 48)]),
            Ok(vec![display("1", 80, 60)]),
        ]);
        let mut state = CaptureState::default();

        capture_tick(
            tmp.path(),
            &mut state,
            &backend,
            &NoopOcrBackend,
            DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp"),
        )
        .expect("first capture");
        capture_tick(
            tmp.path(),
            &mut state,
            &backend,
            &NoopOcrBackend,
            DateTime::from_timestamp(1_700_000_001, 0).expect("timestamp"),
        )
        .expect("second capture");

        let segment_count = WalkDir::new(tmp.path())
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.file_type().is_file()
                    && entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "mp4")
            })
            .count();
        assert_eq!(segment_count, 2);
    }

    #[test]
    fn writes_mp4_segments_without_latest_jpeg_sidecar() {
        let tmp = TempDir::new().expect("tmpdir");
        let backend = SequenceBackend::new(vec![Ok(vec![display("1", 64, 48)])]);
        let mut state = CaptureState::default();

        capture_tick(
            tmp.path(),
            &mut state,
            &backend,
            &NoopOcrBackend,
            DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp"),
        )
        .expect("capture tick");

        let mut mp4_files = WalkDir::new(tmp.path())
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.file_type().is_file()
                    && entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "mp4")
            })
            .map(walkdir::DirEntry::into_path)
            .collect::<Vec<_>>();
        mp4_files.sort();

        assert_eq!(mp4_files.len(), 1);
        assert_eq!(mp4_files[0].parent(), Some(tmp.path()));
        assert!(fs::metadata(&mp4_files[0]).expect("segment metadata").len() > 0);

        let latest_frame_path = mp4_files[0].with_file_name(format!(
            "{}-latest.jpg",
            mp4_files[0]
                .file_stem()
                .and_then(|name| name.to_str())
                .expect("utf8 mp4 stem")
        ));
        assert!(!latest_frame_path.exists());
    }

    #[test]
    fn encodes_physical_frame_dimensions_on_hidpi_displays() {
        let tmp = TempDir::new().expect("tmpdir");
        let backend = SequenceBackend::new(vec![Ok(vec![hidpi_display("1", 64, 48, 2)])]);
        let mut state = CaptureState::default();

        capture_tick(
            tmp.path(),
            &mut state,
            &backend,
            &NoopOcrBackend,
            DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp"),
        )
        .expect("capture tick");

        let manifest_path = WalkDir::new(tmp.path())
            .into_iter()
            .filter_map(Result::ok)
            .find(|entry| {
                entry.file_type().is_file()
                    && entry
                        .path()
                        .to_str()
                        .is_some_and(|path| path.ends_with(".mp4.json"))
            })
            .expect("manifest")
            .into_path();
        let manifest: SegmentManifest =
            serde_json::from_slice(&fs::read(manifest_path).expect("read manifest"))
                .expect("decode manifest");

        assert_eq!(manifest.width, 128);
        assert_eq!(manifest.height, 96);
        assert_eq!(manifest.scale_factor_milli, 2000);
    }

    #[test]
    fn segment_files_are_flat_utc_timestamped_files() {
        let tmp = TempDir::new().expect("tmpdir");
        let backend = SequenceBackend::new(vec![
            Ok(vec![display("1", 64, 48)]),
            Ok(vec![display("1", 64, 48)]),
        ]);
        let mut state = CaptureState::default();

        capture_tick(
            tmp.path(),
            &mut state,
            &backend,
            &NoopOcrBackend,
            DateTime::parse_from_rfc3339("2026-03-23T23:59:59Z")
                .expect("parse")
                .with_timezone(&Utc),
        )
        .expect("first capture");
        capture_tick(
            tmp.path(),
            &mut state,
            &backend,
            &NoopOcrBackend,
            DateTime::parse_from_rfc3339("2026-03-24T00:00:01Z")
                .expect("parse")
                .with_timezone(&Utc),
        )
        .expect("second capture");

        let mut segment_files = WalkDir::new(tmp.path())
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.file_type().is_file()
                    && entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "mp4")
            })
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        segment_files.sort();

        assert_eq!(
            segment_files,
            vec![
                "2026-03-23T23-59-59Z-display-1.mp4".to_string(),
                "2026-03-24T00-00-01Z-display-1.mp4".to_string(),
            ]
        );
    }

    #[test]
    fn prune_removes_old_segments() {
        let tmp = TempDir::new().expect("tmpdir");
        let old_segment = tmp.path().join("2026-03-20T00-00-00Z-display-1.mp4");
        fs::write(&old_segment, b"fake mp4").expect("create old segment");
        let old_manifest = old_segment.with_extension(MANIFEST_FILE_EXTENSION);
        write_manifest(
            &old_manifest,
            &SegmentManifest {
                version: 1,
                display_id: "1".to_string(),
                display_name: "Display 1".to_string(),
                width: 64,
                height: 48,
                rotation_millidegrees: 0,
                scale_factor_milli: 1000,
                segment_started_at: 1_700_000_000,
                started_reason: SegmentReason::Started,
                frame_count: 1,
                newest_frame_at: Some(1_700_000_000),
            },
        )
        .expect("write manifest");

        prune_old_segments(
            tmp.path(),
            DateTime::from_timestamp(1_700_000_000 + RETENTION_SECONDS + 1, 0).expect("timestamp"),
        )
        .expect("prune");

        assert!(!old_segment.exists());
        assert!(!old_manifest.exists());
    }

    #[test]
    fn writes_deduped_ocr_history_per_segment() {
        let tmp = TempDir::new().expect("tmpdir");
        let final_second = (OCR_FRAME_INTERVAL * 2) as i64;
        let backend = SequenceBackend::new(
            (0..=final_second)
                .map(|_| Ok(vec![display("1", 64, 48)]))
                .collect(),
        );
        let ocr_backend = SequenceOcrBackend::new(vec![
            Ok(Some(OcrFrameResult {
                full_text: "Terminal\ncargo test".to_string(),
            })),
            Ok(Some(OcrFrameResult {
                full_text: "Terminal\ncargo test".to_string(),
            })),
            Ok(Some(OcrFrameResult {
                full_text: "Terminal\ncargo test -p codex-app-server".to_string(),
            })),
        ]);
        let mut state = CaptureState::default();

        for second in 0..=final_second {
            capture_tick(
                tmp.path(),
                &mut state,
                &backend,
                &ocr_backend,
                DateTime::from_timestamp(1_700_000_000 + second, 0).expect("timestamp"),
            )
            .expect("capture tick");
        }

        let ocr_path = WalkDir::new(tmp.path())
            .into_iter()
            .filter_map(Result::ok)
            .find(|entry| {
                entry.file_type().is_file()
                    && entry
                        .path()
                        .to_str()
                        .is_some_and(|path| path.ends_with(".ocr.jsonl"))
            })
            .expect("ocr sidecar")
            .into_path();
        let ocr_records = fs::read_to_string(ocr_path)
            .expect("read ocr sidecar")
            .lines()
            .map(|line| serde_json::from_str::<OcrFrameRecord>(line).expect("decode ocr record"))
            .collect::<Vec<_>>();

        assert_eq!(
            ocr_records,
            vec![
                OcrFrameRecord {
                    version: 1,
                    display_id: "1".to_string(),
                    segment_started_at: 1_700_000_000,
                    captured_at: 1_700_000_000,
                    frame_index: 0,
                    full_text: "Terminal\ncargo test".to_string(),
                },
                OcrFrameRecord {
                    version: 1,
                    display_id: "1".to_string(),
                    segment_started_at: 1_700_000_000,
                    captured_at: 1_700_000_000 + final_second,
                    frame_index: OCR_FRAME_INTERVAL * 2,
                    full_text: "Terminal\ncargo test -p codex-app-server".to_string(),
                },
            ]
        );
    }

    #[test]
    fn backend_errors_propagate() {
        let tmp = TempDir::new().expect("tmpdir");
        let backend = SequenceBackend::new(vec![Err(CaptureBackendFailure {
            kind: CaptureBackendFailureKind::PermissionRequired,
            permission: codex_app_server_protocol::ScreenRecordingPermission::Required,
            message: "permission required".to_string(),
        })]);
        let mut state = CaptureState::default();
        let error = capture_tick(
            tmp.path(),
            &mut state,
            &backend,
            &NoopOcrBackend,
            DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp"),
        )
        .expect_err("capture should fail");
        assert_eq!(error.message, "permission required");
    }
}

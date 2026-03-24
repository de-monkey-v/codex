use image::RgbaImage;
use std::io;
use std::sync::Arc;

#[cfg_attr(test, allow(dead_code))]
pub(crate) struct OcrInput<'a> {
    pub(crate) frame: &'a RgbaImage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OcrFrameResult {
    pub(crate) full_text: String,
}

pub(crate) trait OcrBackend: Send + Sync {
    fn recognize(&self, input: OcrInput<'_>) -> io::Result<Option<OcrFrameResult>>;
}

pub(crate) fn default_ocr_backend() -> Arc<dyn OcrBackend> {
    #[cfg(any(test, not(target_os = "macos")))]
    {
        Arc::new(NoopOcrBackend)
    }

    #[cfg(all(not(test), target_os = "macos"))]
    {
        if std::env::var_os(super::backend::FAKE_BACKEND_ENV_VAR).is_none() {
            Arc::new(VisionOcrBackend)
        } else {
            Arc::new(NoopOcrBackend)
        }
    }
}

#[derive(Default)]
pub(crate) struct NoopOcrBackend;

impl OcrBackend for NoopOcrBackend {
    fn recognize(&self, _input: OcrInput<'_>) -> io::Result<Option<OcrFrameResult>> {
        Ok(None)
    }
}

#[cfg(all(not(test), target_os = "macos"))]
struct VisionOcrBackend;

#[cfg(all(not(test), target_os = "macos"))]
impl OcrBackend for VisionOcrBackend {
    fn recognize(&self, input: OcrInput<'_>) -> io::Result<Option<OcrFrameResult>> {
        use objc2::AnyThread;
        use objc2::rc::autoreleasepool;
        use objc2_core_foundation::CFData;
        use objc2_core_graphics::CGBitmapInfo;
        use objc2_core_graphics::CGColorRenderingIntent;
        use objc2_core_graphics::CGColorSpace;
        use objc2_core_graphics::CGDataProvider;
        use objc2_core_graphics::CGImage;
        use objc2_core_graphics::CGImageAlphaInfo;
        use objc2_core_graphics::CGImageByteOrderInfo;
        use objc2_foundation::NSArray;
        use objc2_foundation::NSDictionary;
        use objc2_vision::VNImageRequestHandler;
        use objc2_vision::VNRecognizeTextRequest;
        use objc2_vision::VNRequest;
        use objc2_vision::VNRequestTextRecognitionLevel;

        autoreleasepool(|_| {
            let image_data = CFData::from_bytes(input.frame.as_raw());
            let data_provider = CGDataProvider::with_cf_data(Some(&image_data))
                .ok_or_else(|| io::Error::other("failed to create OCR data provider"))?;
            let color_space = CGColorSpace::new_device_rgb()
                .ok_or_else(|| io::Error::other("failed to create OCR color space"))?;
            let image = unsafe {
                CGImage::new(
                    input.frame.width() as usize,
                    input.frame.height() as usize,
                    8,
                    32,
                    (input.frame.width() * 4) as usize,
                    Some(&color_space),
                    CGBitmapInfo(CGImageByteOrderInfo::Order32Big.0 | CGImageAlphaInfo::Last.0),
                    Some(&data_provider),
                    std::ptr::null(),
                    false,
                    CGColorRenderingIntent::RenderingIntentDefault,
                )
            }
            .ok_or_else(|| io::Error::other("failed to create OCR image"))?;
            let options = NSDictionary::new();
            let handler = unsafe {
                VNImageRequestHandler::initWithCGImage_options(
                    VNImageRequestHandler::alloc(),
                    &image,
                    &options,
                )
            };
            let request = VNRecognizeTextRequest::new();
            request.setRecognitionLevel(VNRequestTextRecognitionLevel::Fast);
            let requests = NSArray::<VNRequest>::from_slice(&[&*request]);
            handler
                .performRequests_error(&requests)
                .map_err(|error| io::Error::other(format!("vision OCR failed: {error:?}")))?;

            let Some(observations) = request.results() else {
                return Ok(None);
            };

            let lines = observations
                .to_vec()
                .into_iter()
                .filter_map(|observation| observation.topCandidates(1).to_vec().into_iter().next())
                .map(|candidate| candidate.string().to_string())
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>();
            if lines.is_empty() {
                return Ok(None);
            }

            Ok(Some(OcrFrameResult {
                full_text: lines.join("\n"),
            }))
        })
    }
}

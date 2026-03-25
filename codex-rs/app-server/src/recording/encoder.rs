#[cfg(target_os = "macos")]
mod imp {
    use muxide::fragmented::FragmentConfig;
    use muxide::fragmented::FragmentedMuxer;
    use objc2_core_foundation::CFArray;
    use objc2_core_foundation::CFBoolean;
    use objc2_core_foundation::CFDictionary;
    use objc2_core_foundation::CFNumber;
    use objc2_core_foundation::CFRetained;
    use objc2_core_foundation::CFString;
    use objc2_core_foundation::CFType;
    use objc2_core_media::CMBlockBuffer;
    use objc2_core_media::CMFormatDescription;
    use objc2_core_media::CMSampleBuffer;
    use objc2_core_media::CMTime;
    use objc2_core_media::CMVideoFormatDescriptionGetH264ParameterSetAtIndex;
    use objc2_core_media::kCMSampleAttachmentKey_NotSync;
    use objc2_core_media::kCMTimeInvalid;
    use objc2_core_media::kCMVideoCodecType_H264;
    use objc2_core_video::CVPixelBuffer;
    use objc2_core_video::CVPixelBufferGetBaseAddress;
    use objc2_core_video::CVPixelBufferGetBytesPerRow;
    use objc2_core_video::CVPixelBufferLockBaseAddress;
    use objc2_core_video::CVPixelBufferLockFlags;
    use objc2_core_video::CVPixelBufferPool;
    use objc2_core_video::CVPixelBufferUnlockBaseAddress;
    use objc2_core_video::kCVPixelBufferHeightKey;
    use objc2_core_video::kCVPixelBufferPixelFormatTypeKey;
    use objc2_core_video::kCVPixelBufferWidthKey;
    use objc2_core_video::kCVPixelFormatType_32BGRA;
    use objc2_video_toolbox::VTCompressionSession;
    use objc2_video_toolbox::VTEncodeInfoFlags;
    use objc2_video_toolbox::VTSession;
    use objc2_video_toolbox::VTSessionSetProperty;
    use objc2_video_toolbox::kVTCompressionPropertyKey_AllowFrameReordering;
    use objc2_video_toolbox::kVTCompressionPropertyKey_ExpectedFrameRate;
    use objc2_video_toolbox::kVTCompressionPropertyKey_MaxKeyFrameInterval;
    use objc2_video_toolbox::kVTCompressionPropertyKey_RealTime;
    use std::fs::File;
    use std::io::Write;
    use std::os::raw::c_void;
    use std::path::Path;
    use std::ptr::NonNull;
    use std::slice;
    use std::sync::Mutex;

    const FRAGMENT_TICKS_PER_FRAME: u64 = 3_000;

    pub(crate) struct FfmpegSegmentEncoder {
        session: Option<CFRetained<VTCompressionSession>>,
        pixel_buffer_pool: Option<CFRetained<CVPixelBufferPool>>,
        callback_state: Option<NonNull<Mutex<CallbackState>>>,
        width: u32,
        height: u32,
        fps: u32,
        next_pts: i64,
        finished: bool,
    }

    struct CallbackState {
        file: File,
        muxer: Option<FragmentedMuxer>,
        next_fragment_tick: u64,
        width: u32,
        height: u32,
        pending_error: Option<std::io::Error>,
    }

    // The encoder is owned by one display stream and all access is serialized through the
    // recording manager's mutex. The callback state is protected by a mutex and the session is
    // only driven from one owner thread.
    unsafe impl Send for FfmpegSegmentEncoder {}

    impl FfmpegSegmentEncoder {
        pub(crate) fn open(
            path: &Path,
            width: u32,
            height: u32,
            fps: u32,
        ) -> std::io::Result<Self> {
            let callback_state = NonNull::from(Box::leak(Box::new(Mutex::new(CallbackState {
                file: File::create(path)?,
                muxer: None,
                next_fragment_tick: 0,
                width,
                height,
                pending_error: None,
            }))));
            let source_attributes = pixel_buffer_attributes(width, height);

            let mut raw_session = std::ptr::null_mut();
            let status = unsafe {
                VTCompressionSession::create(
                    None,
                    width as i32,
                    height as i32,
                    kCMVideoCodecType_H264,
                    None,
                    Some(source_attributes.as_ref()),
                    None,
                    Some(compression_output_callback),
                    callback_state.as_ptr().cast::<c_void>(),
                    NonNull::from(&mut raw_session),
                )
            };
            if status != 0 {
                unsafe {
                    drop(Box::from_raw(callback_state.as_ptr()));
                }
                return Err(os_status_error(
                    status,
                    "failed to create VideoToolbox compression session",
                ));
            }

            let Some(raw_session) = NonNull::new(raw_session) else {
                unsafe {
                    drop(Box::from_raw(callback_state.as_ptr()));
                }
                return Err(std::io::Error::other(
                    "VideoToolbox returned a null compression session",
                ));
            };
            let session = unsafe { CFRetained::from_raw(raw_session) };

            set_session_property(
                session.as_ref(),
                unsafe { kVTCompressionPropertyKey_RealTime },
                CFBoolean::new(true).as_ref(),
            )?;
            set_session_property(
                session.as_ref(),
                unsafe { kVTCompressionPropertyKey_AllowFrameReordering },
                CFBoolean::new(false).as_ref(),
            )?;
            let expected_frame_rate = CFNumber::new_i32(fps as i32);
            set_session_property(
                session.as_ref(),
                unsafe { kVTCompressionPropertyKey_ExpectedFrameRate },
                expected_frame_rate.as_ref(),
            )?;
            let max_key_frame_interval = CFNumber::new_i32((fps.saturating_mul(10)) as i32);
            set_session_property(
                session.as_ref(),
                unsafe { kVTCompressionPropertyKey_MaxKeyFrameInterval },
                max_key_frame_interval.as_ref(),
            )?;

            let status = unsafe { session.prepare_to_encode_frames() };
            if status != 0 {
                unsafe {
                    session.invalidate();
                    drop(Box::from_raw(callback_state.as_ptr()));
                }
                return Err(os_status_error(
                    status,
                    "failed to prepare VideoToolbox compression session",
                ));
            }
            let pixel_buffer_pool = unsafe { session.pixel_buffer_pool() }.ok_or_else(|| {
                unsafe {
                    session.invalidate();
                    drop(Box::from_raw(callback_state.as_ptr()));
                }
                std::io::Error::other("VideoToolbox did not expose a source pixel buffer pool")
            })?;

            Ok(Self {
                session: Some(session),
                pixel_buffer_pool: Some(pixel_buffer_pool),
                callback_state: Some(callback_state),
                width,
                height,
                fps,
                next_pts: 0,
                finished: false,
            })
        }

        pub(crate) fn write_rgba_frame(&mut self, rgba: &[u8]) -> std::io::Result<()> {
            let expected_len = self.width as usize * self.height as usize * 4;
            if rgba.len() != expected_len {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "rgba frame length {} did not match expected {expected_len}",
                        rgba.len()
                    ),
                ));
            }

            let Some(session) = self.session.as_ref() else {
                return Err(std::io::Error::other("encoder session already finished"));
            };
            let Some(pixel_buffer_pool) = self.pixel_buffer_pool.as_ref() else {
                return Err(std::io::Error::other(
                    "encoder pixel buffer pool already finished",
                ));
            };
            let pixel_buffer =
                create_pixel_buffer(pixel_buffer_pool.as_ref(), self.width, self.height, rgba)?;
            let presentation_time_stamp = unsafe { CMTime::new(self.next_pts, self.fps as i32) };
            let duration = unsafe { CMTime::new(1, self.fps as i32) };
            let mut info_flags = VTEncodeInfoFlags(0);
            let status = unsafe {
                session.encode_frame(
                    pixel_buffer.as_ref(),
                    presentation_time_stamp,
                    duration,
                    None,
                    std::ptr::null_mut(),
                    &mut info_flags,
                )
            };
            if status != 0 {
                return Err(os_status_error(
                    status,
                    "failed to encode VideoToolbox frame",
                ));
            }

            let status = unsafe { session.complete_frames(kCMTimeInvalid) };
            if status != 0 {
                return Err(os_status_error(
                    status,
                    "failed to flush VideoToolbox encoded frame",
                ));
            }

            self.next_pts += 1;
            self.take_callback_error()
        }

        pub(crate) fn finish(&mut self) -> std::io::Result<()> {
            if self.finished {
                return Ok(());
            }

            if let Some(session) = self.session.take() {
                let status = unsafe { session.complete_frames(kCMTimeInvalid) };
                if status != 0 {
                    return Err(os_status_error(
                        status,
                        "failed to flush pending VideoToolbox frames",
                    ));
                }
                self.take_callback_error()?;
                unsafe {
                    session.invalidate();
                }
            }
            self.pixel_buffer_pool = None;

            if let Some(callback_state) = self.callback_state.take() {
                let mut callback_state = unsafe { Box::from_raw(callback_state.as_ptr()) };
                let state = callback_state
                    .get_mut()
                    .map_err(|_| std::io::Error::other("recording callback mutex poisoned"))?;
                state.file.flush()?;
                if let Some(error) = state.pending_error.take() {
                    return Err(error);
                }
            }

            self.finished = true;
            Ok(())
        }

        fn take_callback_error(&mut self) -> std::io::Result<()> {
            let Some(callback_state) = self.callback_state else {
                return Ok(());
            };
            let mut state = unsafe { callback_state.as_ref() }
                .lock()
                .map_err(|_| std::io::Error::other("recording callback mutex poisoned"))?;
            if let Some(error) = state.pending_error.take() {
                return Err(error);
            }
            Ok(())
        }
    }

    impl Drop for FfmpegSegmentEncoder {
        fn drop(&mut self) {
            if let Err(error) = self.finish() {
                tracing::debug!("failed to finish mp4 segment: {error}");
            }
        }
    }

    unsafe extern "C-unwind" fn compression_output_callback(
        output_callback_ref_con: *mut c_void,
        _source_frame_ref_con: *mut c_void,
        status: i32,
        info_flags: VTEncodeInfoFlags,
        sample_buffer: *mut CMSampleBuffer,
    ) {
        let Some(callback_state) =
            NonNull::new(output_callback_ref_con.cast::<Mutex<CallbackState>>())
        else {
            return;
        };
        let Ok(mut state) = unsafe { callback_state.as_ref() }.lock() else {
            return;
        };

        if status != 0 {
            record_callback_error(
                &mut state,
                os_status_error(status, "VideoToolbox encode callback failed"),
            );
            return;
        }
        if info_flags.contains(VTEncodeInfoFlags::FrameDropped) {
            record_callback_error(
                &mut state,
                std::io::Error::other("VideoToolbox dropped a recording frame"),
            );
            return;
        }

        let Some(sample_buffer) = NonNull::new(sample_buffer) else {
            record_callback_error(
                &mut state,
                std::io::Error::other("VideoToolbox callback returned a null sample buffer"),
            );
            return;
        };

        if let Err(error) = write_encoded_sample(&mut state, unsafe { sample_buffer.as_ref() }) {
            record_callback_error(&mut state, error);
        }
    }

    fn write_encoded_sample(
        state: &mut CallbackState,
        sample_buffer: &CMSampleBuffer,
    ) -> std::io::Result<()> {
        let sample_data = sample_buffer_data(sample_buffer)?;
        if state.muxer.is_none() {
            let (sps, pps) = sample_parameter_sets(sample_buffer)?;
            let mut muxer = FragmentedMuxer::new(FragmentConfig {
                width: state.width,
                height: state.height,
                timescale: FRAGMENT_TICKS_PER_FRAME as u32,
                fragment_duration_ms: 0,
                sps,
                pps,
                vps: None,
                av1_sequence_header: None,
                vp9_config: None,
            });
            let init_segment = muxer.init_segment();
            state.file.write_all(&init_segment)?;
            state.file.flush()?;
            state.muxer = Some(muxer);
        }

        let is_sync = sample_is_sync(sample_buffer)?;
        let Some(muxer) = state.muxer.as_mut() else {
            return Err(std::io::Error::other(
                "fragmented muxer was not initialized before writing a sample",
            ));
        };
        let sample_tick = state.next_fragment_tick;
        muxer
            .write_video(sample_tick, sample_tick, &sample_data, is_sync)
            .map_err(std::io::Error::other)?;
        let segment = muxer
            .flush_segment()
            .ok_or_else(|| std::io::Error::other("fragmented muxer did not emit a segment"))?;
        state.file.write_all(&segment)?;
        state.file.flush()?;
        state.next_fragment_tick = state
            .next_fragment_tick
            .saturating_add(FRAGMENT_TICKS_PER_FRAME);
        Ok(())
    }

    fn record_callback_error(state: &mut CallbackState, error: std::io::Error) {
        if state.pending_error.is_none() {
            state.pending_error = Some(error);
        }
    }

    fn pixel_buffer_attributes(
        width: u32,
        height: u32,
    ) -> CFRetained<CFDictionary<CFString, CFType>> {
        let width_number = CFNumber::new_i32(width as i32);
        let height_number = CFNumber::new_i32(height as i32);
        let pixel_format = CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32);
        CFDictionary::from_slices(
            &[
                unsafe { kCVPixelBufferWidthKey },
                unsafe { kCVPixelBufferHeightKey },
                unsafe { kCVPixelBufferPixelFormatTypeKey },
            ],
            &[
                width_number.as_ref(),
                height_number.as_ref(),
                pixel_format.as_ref(),
            ],
        )
    }

    fn create_pixel_buffer(
        pixel_buffer_pool: &CVPixelBufferPool,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> std::io::Result<CFRetained<CVPixelBuffer>> {
        let mut raw_pixel_buffer = std::ptr::null_mut();
        let status = unsafe {
            CVPixelBufferPool::create_pixel_buffer(
                None,
                pixel_buffer_pool,
                NonNull::from(&mut raw_pixel_buffer),
            )
        };
        if status != 0 {
            return Err(os_status_error(
                status,
                "failed to allocate CoreVideo pixel buffer",
            ));
        }
        let raw_pixel_buffer = NonNull::new(raw_pixel_buffer)
            .ok_or_else(|| std::io::Error::other("CoreVideo returned a null pixel buffer"))?;
        let pixel_buffer = unsafe { CFRetained::from_raw(raw_pixel_buffer) };

        let status = unsafe {
            CVPixelBufferLockBaseAddress(pixel_buffer.as_ref(), CVPixelBufferLockFlags(0))
        };
        if status != 0 {
            return Err(os_status_error(
                status,
                "failed to lock CoreVideo pixel buffer",
            ));
        }

        let row_bytes = width as usize * 4;
        let copy_result: std::io::Result<()> = (|| {
            let bytes_per_row = CVPixelBufferGetBytesPerRow(pixel_buffer.as_ref());
            let base_address = CVPixelBufferGetBaseAddress(pixel_buffer.as_ref());
            let base_address = NonNull::new(base_address.cast::<u8>())
                .ok_or_else(|| std::io::Error::other("pixel buffer base address was null"))?;
            for row in 0..height as usize {
                let src_start = row * row_bytes;
                let dst_start = row * bytes_per_row;
                let dst_row = unsafe {
                    slice::from_raw_parts_mut(base_address.as_ptr().add(dst_start), row_bytes)
                };
                for (dst_pixel, src_pixel) in dst_row
                    .chunks_exact_mut(4)
                    .zip(rgba[src_start..src_start + row_bytes].chunks_exact(4))
                {
                    dst_pixel[0] = src_pixel[2];
                    dst_pixel[1] = src_pixel[1];
                    dst_pixel[2] = src_pixel[0];
                    dst_pixel[3] = src_pixel[3];
                }
            }
            Ok(())
        })();

        let unlock_status = unsafe {
            CVPixelBufferUnlockBaseAddress(pixel_buffer.as_ref(), CVPixelBufferLockFlags(0))
        };
        if unlock_status != 0 {
            return Err(os_status_error(
                unlock_status,
                "failed to unlock CoreVideo pixel buffer",
            ));
        }
        copy_result?;

        Ok(pixel_buffer)
    }

    fn set_session_property(
        session: &VTCompressionSession,
        key: &CFString,
        value: &CFType,
    ) -> std::io::Result<()> {
        let status = unsafe {
            VTSessionSetProperty(
                &*(session as *const VTCompressionSession as *const VTSession),
                key,
                Some(value),
            )
        };
        if status != 0 {
            return Err(os_status_error(
                status,
                &format!("failed to set VideoToolbox property {key}"),
            ));
        }
        Ok(())
    }

    fn sample_buffer_data(sample_buffer: &CMSampleBuffer) -> std::io::Result<Vec<u8>> {
        let data_buffer = unsafe { sample_buffer.data_buffer() }.ok_or_else(|| {
            std::io::Error::other("encoded sample buffer was missing a data buffer")
        })?;
        contiguous_block_buffer_bytes(data_buffer.as_ref())
    }

    fn contiguous_block_buffer_bytes(block_buffer: &CMBlockBuffer) -> std::io::Result<Vec<u8>> {
        let mut length_at_offset = 0usize;
        let mut total_length = 0usize;
        let mut data_pointer = std::ptr::null_mut();
        let status = unsafe {
            block_buffer.data_pointer(
                0,
                &mut length_at_offset,
                &mut total_length,
                &mut data_pointer,
            )
        };
        if status != 0 {
            return Err(os_status_error(
                status,
                "failed to access encoded block buffer bytes",
            ));
        }
        if length_at_offset < total_length {
            return Err(std::io::Error::other(
                "encoded block buffer was not stored contiguously",
            ));
        }
        let data_pointer = NonNull::new(data_pointer.cast::<u8>())
            .ok_or_else(|| std::io::Error::other("encoded block buffer pointer was null"))?;
        Ok(unsafe { slice::from_raw_parts(data_pointer.as_ptr(), total_length) }.to_vec())
    }

    fn sample_parameter_sets(
        sample_buffer: &CMSampleBuffer,
    ) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
        let format_description =
            unsafe { sample_buffer.format_description() }.ok_or_else(|| {
                std::io::Error::other("encoded sample buffer was missing a format description")
            })?;
        h264_parameter_sets(format_description.as_ref())
    }

    fn h264_parameter_sets(
        format_description: &CMFormatDescription,
    ) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
        let mut parameter_set_count = 0usize;
        let status = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format_description,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut parameter_set_count,
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(os_status_error(
                status,
                "failed to inspect H.264 parameter set count",
            ));
        }

        let mut sps = None;
        let mut pps = None;
        for index in 0..parameter_set_count {
            let mut parameter_set_pointer = std::ptr::null();
            let mut parameter_set_size = 0usize;
            let status = unsafe {
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    format_description,
                    index,
                    &mut parameter_set_pointer,
                    &mut parameter_set_size,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if status != 0 {
                return Err(os_status_error(
                    status,
                    "failed to read H.264 parameter set",
                ));
            }
            let parameter_set_pointer = NonNull::new(parameter_set_pointer.cast_mut())
                .ok_or_else(|| std::io::Error::other("parameter set pointer was null"))?;
            let parameter_set = unsafe {
                slice::from_raw_parts(
                    parameter_set_pointer.as_ptr().cast::<u8>(),
                    parameter_set_size,
                )
            };
            match parameter_set.first().map(|nal| nal & 0x1f) {
                Some(7) => sps = Some(parameter_set.to_vec()),
                Some(8) => pps = Some(parameter_set.to_vec()),
                _ => {}
            }
        }

        match (sps, pps) {
            (Some(sps), Some(pps)) => Ok((sps, pps)),
            _ => Err(std::io::Error::other(
                "encoded sample buffer did not expose SPS/PPS parameter sets",
            )),
        }
    }

    fn sample_is_sync(sample_buffer: &CMSampleBuffer) -> std::io::Result<bool> {
        let Some(attachments) = (unsafe { sample_buffer.sample_attachments_array(false) }) else {
            return Ok(true);
        };
        let attachments_ref: &CFArray = attachments.as_ref();
        let attachments =
            unsafe { attachments_ref.cast_unchecked::<CFDictionary<CFString, CFType>>() };
        let Some(first_attachment): Option<CFRetained<CFDictionary<CFString, CFType>>> =
            attachments.get(0)
        else {
            return Ok(true);
        };
        Ok(!first_attachment.contains_key(unsafe { kCMSampleAttachmentKey_NotSync }))
    }

    fn os_status_error(status: i32, context: &str) -> std::io::Error {
        std::io::Error::other(format!("{context} (OSStatus {status})"))
    }

    #[cfg(test)]
    mod tests {
        use super::FfmpegSegmentEncoder;
        use pretty_assertions::assert_eq;
        use tempfile::NamedTempFile;

        #[test]
        fn writes_fragmented_mp4() {
            let file = NamedTempFile::new().expect("tempfile");
            let mut encoder =
                FfmpegSegmentEncoder::open(file.path(), 2, 2, 1).expect("open encoder");
            let frame = [
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ];

            encoder.write_rgba_frame(&frame).expect("write first frame");
            let len_after_first_frame = std::fs::metadata(file.path())
                .expect("first metadata")
                .len();
            assert!(len_after_first_frame > 0);

            encoder
                .write_rgba_frame(&frame)
                .expect("write second frame");
            encoder.finish().expect("finish encoder");
            let final_len = std::fs::metadata(file.path())
                .expect("final metadata")
                .len();
            assert!(final_len >= len_after_first_frame);
            assert_eq!(
                std::fs::read(file.path()).expect("read segment")[4..8],
                *b"ftyp"
            );
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;

    pub(crate) struct FfmpegSegmentEncoder {
        file: File,
        finished: bool,
    }

    impl FfmpegSegmentEncoder {
        pub(crate) fn open(
            path: &Path,
            _width: u32,
            _height: u32,
            _fps: u32,
        ) -> std::io::Result<Self> {
            // Non-macOS builds do not support recording at runtime, but persistence tests still
            // exercise segment rotation and manifest handling. Emit a small placeholder file so
            // those tests can validate the file lifecycle without pulling FFmpeg into the build.
            let mut file = File::create(path)?;
            file.write_all(b"placeholder mp4 segment\n")?;
            file.sync_all()?;
            Ok(Self {
                file,
                finished: false,
            })
        }

        pub(crate) fn write_rgba_frame(&mut self, rgba: &[u8]) -> std::io::Result<()> {
            let bytes_to_copy = rgba.len().min(4096);
            self.file.write_all(&(rgba.len() as u64).to_le_bytes())?;
            self.file.write_all(&rgba[..bytes_to_copy])?;
            self.file.flush()
        }

        pub(crate) fn finish(&mut self) -> std::io::Result<()> {
            if self.finished {
                return Ok(());
            }
            self.file.sync_all()?;
            self.finished = true;
            Ok(())
        }
    }

    impl Drop for FfmpegSegmentEncoder {
        fn drop(&mut self) {
            if let Err(error) = self.finish() {
                tracing::debug!("failed to finish placeholder mp4 segment: {error}");
            }
        }
    }
}

pub(crate) use imp::FfmpegSegmentEncoder;

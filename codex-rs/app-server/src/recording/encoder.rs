use ffmpeg::Dictionary;
use ffmpeg::Packet;
use ffmpeg::codec;
use ffmpeg::encoder;
use ffmpeg::format;
use ffmpeg::frame;
use ffmpeg::picture;
use ffmpeg::software::scaling;
use ffmpeg::util::format::pixel::Pixel;
use ffmpeg_next as ffmpeg;
use std::path::Path;
use std::sync::Once;

static FFMPEG_INIT: Once = Once::new();

pub(crate) struct FfmpegSegmentEncoder {
    output: format::context::Output,
    encoder: encoder::Video,
    scaler: scaling::context::Context,
    stream_index: usize,
    stream_time_base: ffmpeg::Rational,
    width: u32,
    height: u32,
    next_pts: i64,
    finished: bool,
}

// The encoder is owned by one display stream and all access is serialized through the
// recording manager's mutex. FFmpeg's Rust bindings do not mark the underlying contexts as
// `Send` because they contain raw pointers, but we only move the encoder between threads and
// never access it concurrently.
unsafe impl Send for FfmpegSegmentEncoder {}

impl FfmpegSegmentEncoder {
    pub(crate) fn open(path: &Path, width: u32, height: u32, fps: u32) -> std::io::Result<Self> {
        FFMPEG_INIT.call_once(|| {
            if let Err(error) = ffmpeg::init() {
                panic!("failed to initialize ffmpeg: {error}");
            }
            ffmpeg::log::set_level(ffmpeg::log::Level::Error);
        });

        let mut output = format::output(path).map_err(std::io::Error::other)?;
        let codec = encoder::find(codec::Id::H264)
            .ok_or_else(|| std::io::Error::other("H.264 encoder not found"))?;
        let global_header = output
            .format()
            .flags()
            .contains(format::Flags::GLOBAL_HEADER);

        let mut video = codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()
            .map_err(std::io::Error::other)?;
        video.set_width(width);
        video.set_height(height);
        video.set_format(Pixel::YUV420P);
        video.set_time_base((1, fps as i32));
        video.set_frame_rate(Some((fps as i32, 1)));
        video.set_gop(fps * 10);
        video.set_max_b_frames(0);
        if global_header {
            video.set_flags(codec::flag::Flags::GLOBAL_HEADER);
        }

        let mut options = Dictionary::new();
        options.set("preset", "veryfast");
        options.set("crf", "28");
        options.set("tune", "zerolatency");
        let encoder = video.open_with(options).map_err(std::io::Error::other)?;

        let stream_index = {
            let mut stream = output.add_stream(codec).map_err(std::io::Error::other)?;
            stream.set_time_base((1, fps as i32));
            stream.set_parameters(&encoder);
            stream.index()
        };

        let mut muxer_options = Dictionary::new();
        muxer_options.set("movflags", "frag_keyframe+empty_moov+default_base_moof");
        output
            .write_header_with(muxer_options)
            .map_err(std::io::Error::other)?;
        let stream_time_base = output
            .stream(stream_index)
            .ok_or_else(|| std::io::Error::other("encoded stream missing after header write"))?
            .time_base();

        let scaler = scaling::context::Context::get(
            Pixel::RGBA,
            width,
            height,
            Pixel::YUV420P,
            width,
            height,
            scaling::flag::Flags::BILINEAR,
        )
        .map_err(std::io::Error::other)?;

        Ok(Self {
            output,
            encoder,
            scaler,
            stream_index,
            stream_time_base,
            width,
            height,
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

        let mut rgba_frame = frame::Video::new(Pixel::RGBA, self.width, self.height);
        let stride = rgba_frame.stride(0);
        let row_bytes = self.width as usize * 4;
        {
            let data = rgba_frame.data_mut(0);
            for row in 0..self.height as usize {
                let src_start = row * row_bytes;
                let dst_start = row * stride;
                data[dst_start..dst_start + row_bytes]
                    .copy_from_slice(&rgba[src_start..src_start + row_bytes]);
            }
        }

        let mut yuv_frame = frame::Video::new(Pixel::YUV420P, self.width, self.height);
        self.scaler
            .run(&rgba_frame, &mut yuv_frame)
            .map_err(std::io::Error::other)?;
        yuv_frame.set_pts(Some(self.next_pts));
        yuv_frame.set_kind(picture::Type::None);
        self.next_pts += 1;

        self.encoder
            .send_frame(&yuv_frame)
            .map_err(std::io::Error::other)?;
        self.drain_packets()
    }

    pub(crate) fn finish(&mut self) -> std::io::Result<()> {
        if self.finished {
            return Ok(());
        }

        match self.encoder.send_eof() {
            Ok(()) | Err(ffmpeg::Error::Eof) => {}
            Err(error) => return Err(std::io::Error::other(error)),
        }
        self.drain_packets()?;
        self.output.write_trailer().map_err(std::io::Error::other)?;
        self.finished = true;
        Ok(())
    }

    fn drain_packets(&mut self) -> std::io::Result<()> {
        let mut packet = Packet::empty();
        loop {
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    packet.set_stream(self.stream_index);
                    packet.rescale_ts(self.encoder.time_base(), self.stream_time_base);
                    packet
                        .write_interleaved(&mut self.output)
                        .map_err(std::io::Error::other)?;
                }
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => break,
                Err(ffmpeg::Error::Eof) => break,
                Err(error) => return Err(std::io::Error::other(error)),
            }
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

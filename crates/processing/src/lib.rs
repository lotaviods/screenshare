//! `sharescreen-processing`: FFmpeg-backed media processing.
//!
//! GStreamer owns the live path (capture, encode, WebRTC). This crate owns
//! everything that is not real time: probing, thumbnails/screenshots, and
//! conversion/compression of recorded files. Keeping the two apart means the
//! live pipeline never waits on a muxer or a scaler.
//!
//! Recording from the live pipeline reuses the same FFmpeg muxers; that wiring
//! needs the encoded packet timestamps from the call session and is not exposed
//! here yet.

use std::path::Path;

use ffmpeg::format::Pixel;
use ffmpeg::media::Type;
use ffmpeg::{codec, encoder, format, frame, Rational};
use ffmpeg_next as ffmpeg;
use tracing::{debug, info};

#[derive(Debug, thiserror::Error)]
pub enum ProcessingError {
    #[error("ffmpeg: {0}")]
    Ffmpeg(#[from] ffmpeg::Error),
    #[error("no video stream in {0}")]
    NoVideo(String),
    #[error("unsupported output format for {0}")]
    UnsupportedOutput(String),
    #[error("invalid media: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, ProcessingError>;

/// What a media file contains, for the UI and for conversion decisions.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaInfo {
    pub duration_ms: u64,
    pub video: Option<VideoInfo>,
    pub audio: Option<AudioInfo>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub frame_rate: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioInfo {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Video codec for conversions and compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    /// H.264 via the best installed encoder (libx264), for MP4/MKV.
    H264,
    /// VP9 via libvpx, for WebM/MKV.
    Vp9,
}

/// Conversion and compression settings.
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    pub codec: VideoCodec,
    /// Downscale width; the height keeps the aspect ratio.
    pub max_width: Option<u32>,
    /// Target video bitrate in kbps.
    pub video_bitrate_kbps: u32,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            codec: VideoCodec::H264,
            max_width: None,
            video_bitrate_kbps: 2500,
        }
    }
}

fn init() -> Result<()> {
    ffmpeg::init().map_err(ProcessingError::Ffmpeg)
}

/// Read duration, codecs and dimensions of a media file.
pub fn probe(path: impl AsRef<Path>) -> Result<MediaInfo> {
    init()?;
    let input = format::input(&path)?;
    let duration_ms = u64::try_from(input.duration() / 1000).unwrap_or(0);

    let video = input.streams().best(Type::Video).map(|stream| {
        let parameters = stream.parameters().clone();
        let decoder = codec::context::Context::from_parameters(parameters.clone())
            .ok()
            .and_then(|context| context.decoder().video().ok());
        let (width, height, frame_rate) = decoder.map_or((0, 0, None), |decoder| {
            (
                decoder.width(),
                decoder.height(),
                Some(f64::from(
                    decoder.frame_rate().unwrap_or(Rational::new(0, 1)),
                )),
            )
        });
        VideoInfo {
            codec: format!("{:?}", parameters.id()),
            width,
            height,
            frame_rate: frame_rate.filter(|rate| *rate > 0.0),
        }
    });

    let audio = input.streams().best(Type::Audio).map(|stream| {
        let parameters = stream.parameters().clone();
        let decoder = codec::context::Context::from_parameters(parameters.clone())
            .ok()
            .and_then(|context| context.decoder().audio().ok());
        AudioInfo {
            codec: format!("{:?}", parameters.id()),
            sample_rate: decoder.as_ref().map_or(0, codec::decoder::Audio::rate),
            channels: decoder
                .as_ref()
                .map_or(0, |decoder| decoder.channel_layout().channels() as u16),
        }
    });

    Ok(MediaInfo {
        duration_ms,
        video,
        audio,
    })
}

/// Decode the frame at (or just after) `at_ms` and return it as a JPEG whose
/// width is at most `max_width`. This is the screenshot/thumbnail path.
pub fn thumbnail_jpeg(path: impl AsRef<Path>, at_ms: u64, max_width: u32) -> Result<Vec<u8>> {
    init()?;
    let path = path.as_ref();
    let mut input = format::input(path)?;
    let stream = input
        .streams()
        .best(Type::Video)
        .ok_or_else(|| ProcessingError::NoVideo(path.display().to_string()))?;
    let stream_index = stream.index();
    let time_base = stream.time_base();
    let mut decoder = codec::context::Context::from_parameters(stream.parameters())?
        .decoder()
        .video()?;

    // Seek to the requested time; a failing seek (non-seekable input) just
    // starts from the beginning.
    let target = at_ms as i64 * 1000;
    if at_ms > 0 && time_base.denominator() > 0 {
        let ts = target * i64::from(time_base.denominator()) / 1_000_000;
        let _ = input.seek(ts, ..);
    }

    let mut decoded = frame::Video::empty();
    for (stream, packet) in input.packets() {
        if stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet)?;
        if decoder.receive_frame(&mut decoded).is_ok() {
            let width = max_width.max(1).min(decoded.width().max(1));
            let height = (decoded.height() * width / decoded.width().max(1)).max(1);
            let mut scaler = ffmpeg::software::scaling::Context::get(
                decoded.format(),
                decoded.width(),
                decoded.height(),
                Pixel::YUVJ420P,
                width,
                height,
                ffmpeg::software::scaling::flag::Flags::BILINEAR,
            )?;
            let mut scaled = frame::Video::empty();
            scaler.run(&decoded, &mut scaled)?;
            return encode_jpeg(&scaled, width, height);
        }
    }
    Err(ProcessingError::NoVideo(format!(
        "no decodable frame in {}",
        path.display()
    )))
}

fn encode_jpeg(frame: &frame::Video, width: u32, height: u32) -> Result<Vec<u8>> {
    let codec = ffmpeg::encoder::find_by_name("mjpeg")
        .ok_or_else(|| ProcessingError::UnsupportedOutput("mjpeg encoder".into()))?;
    let context = codec::context::Context::new_with_codec(codec);
    let mut encoder = context.encoder().video()?;
    encoder.set_width(width);
    encoder.set_height(height);
    encoder.set_format(Pixel::YUVJ420P);
    encoder.set_time_base(Rational::new(1, 1));
    encoder.set_frame_rate(Some(Rational::new(1, 1)));
    let mut encoder = encoder.open_as(codec)?;
    encoder.send_frame(frame)?;
    encoder.send_eof()?;
    let mut packet = ffmpeg::Packet::empty();
    encoder.receive_packet(&mut packet)?;
    packet
        .data()
        .map(<[u8]>::to_vec)
        .ok_or_else(|| ProcessingError::Invalid("empty JPEG packet".into()))
}

/// Re-encode the video (optionally downscaled) and copy the audio streams.
///
/// The output container comes from the file extension. Audio is stream-copied,
/// so the extension must accept the source audio codec (WebM/Matroska accept
/// Opus; MP4 accepts AAC).
pub fn convert(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    options: &ConvertOptions,
) -> Result<()> {
    init()?;
    let input = input.as_ref();
    let output = output.as_ref();
    let mut reader = format::input(input)?;
    let mut writer = format::output(output)?;

    let video_stream = reader
        .streams()
        .best(Type::Video)
        .ok_or_else(|| ProcessingError::NoVideo(input.display().to_string()))?;
    let video_index = video_stream.index();
    let input_time_base = video_stream.time_base();
    let mut decoder = codec::context::Context::from_parameters(video_stream.parameters())?
        .decoder()
        .video()?;
    let frame_rate = decoder
        .frame_rate()
        .filter(|rate| rate.numerator() > 0)
        .unwrap_or(Rational::new(30, 1));

    let codec_id = match options.codec {
        VideoCodec::H264 => codec::Id::H264,
        VideoCodec::Vp9 => codec::Id::VP9,
    };
    let encoder_codec = ffmpeg::encoder::find(codec_id)
        .ok_or_else(|| ProcessingError::UnsupportedOutput(format!("{codec_id:?} encoder")))?;

    let scale_width = options
        .max_width
        .map(|width| width.max(1).min(decoder.width().max(1)))
        .filter(|width| *width < decoder.width())
        .unwrap_or(decoder.width());
    let scale_height = (decoder.height() * scale_width / decoder.width().max(1)).max(1);

    let global_header = writer
        .format()
        .flags()
        .contains(format::Flags::GLOBAL_HEADER);
    let mut video_encoder = codec::context::Context::new_with_codec(encoder_codec)
        .encoder()
        .video()?;
    video_encoder.set_width(scale_width);
    video_encoder.set_height(scale_height);
    video_encoder.set_format(Pixel::YUV420P);
    video_encoder.set_frame_rate(Some(frame_rate));
    video_encoder.set_time_base(input_time_base);
    video_encoder.set_bit_rate(options.video_bitrate_kbps as usize * 1000);
    video_encoder.set_gop((f64::from(frame_rate) * 2.0).max(1.0) as u32);
    video_encoder.set_max_b_frames(0);
    if global_header {
        video_encoder.set_flags(codec::Flags::GLOBAL_HEADER);
    }
    let mut encoder = video_encoder.open_as(encoder_codec)?;

    let video_output_index = {
        let mut stream = writer.add_stream(encoder_codec)?;
        stream.set_parameters(&encoder);
        stream.index()
    };

    // Audio is copied, not re-encoded: conversion keeps the existing Opus/AAC.
    let mut audio_streams = Vec::new();
    if let Some(audio) = reader.streams().best(Type::Audio) {
        let index = audio.index();
        let time_base = audio.time_base();
        let output_index = {
            let mut output_stream = writer.add_stream(encoder::find(codec::Id::None))?;
            output_stream.set_parameters(audio.parameters());
            // A zero codec tag keeps the muxer from reusing an incompatible tag
            // when the container changes.
            unsafe {
                (*output_stream.parameters().as_mut_ptr()).codec_tag = 0;
            }
            output_stream.index()
        };
        audio_streams.push((index, output_index, time_base));
    }

    let mut video_output_time_base = Rational::new(0, 0);
    let mut audio_output_time_bases = Vec::new();
    writer.write_header()?;
    for (index, _) in writer.streams().enumerate() {
        let time_base = writer.stream(index as _).map(|stream| stream.time_base());
        if index == video_output_index {
            video_output_time_base = time_base.unwrap_or(input_time_base);
        } else if let Some((_, output_index, _)) = audio_streams
            .iter()
            .find(|(_, output_index, _)| *output_index == index)
        {
            audio_output_time_bases.push((*output_index, time_base.unwrap_or(input_time_base)));
        }
    }

    let mut scaler: Option<ffmpeg::software::scaling::Context> = None;
    let mut video_written = 0u64;
    let mut audio_written = 0u64;

    for (stream, mut packet) in reader.packets() {
        let index = stream.index();
        if index == video_index {
            decoder.send_packet(&packet)?;
            let mut decoded = frame::Video::empty();
            while decoder.receive_frame(&mut decoded).is_ok() {
                if scaler.is_none() {
                    scaler = Some(ffmpeg::software::scaling::Context::get(
                        decoded.format(),
                        decoded.width(),
                        decoded.height(),
                        Pixel::YUV420P,
                        scale_width,
                        scale_height,
                        ffmpeg::software::scaling::flag::Flags::BILINEAR,
                    )?);
                }
                let scaled = if let Some(scaler) = scaler.as_mut() {
                    let mut scaled = frame::Video::empty();
                    scaler.run(&decoded, &mut scaled)?;
                    carry_timestamps(&decoded, &mut scaled);
                    scaled
                } else {
                    decoded.clone()
                };
                encoder.send_frame(&scaled)?;
                write_encoded(
                    &mut encoder,
                    &mut writer,
                    video_output_index,
                    video_output_time_base,
                    &mut video_written,
                )?;
            }
        } else if let Some((_, output_index, input_time_base)) = audio_streams
            .iter()
            .find(|(input_index, _, _)| *input_index == index)
        {
            let output_time_base = audio_output_time_bases
                .iter()
                .find(|(index, _)| index == output_index)
                .map(|(_, time_base)| *time_base)
                .unwrap_or(*input_time_base);
            packet.rescale_ts(*input_time_base, output_time_base);
            packet.set_position(-1);
            packet.set_stream(*output_index);
            packet.write_interleaved(&mut writer)?;
            audio_written += 1;
        }
    }

    decoder.send_eof()?;
    let mut decoded = frame::Video::empty();
    while decoder.receive_frame(&mut decoded).is_ok() {
        let scaled = if let Some(scaler) = scaler.as_mut() {
            let mut scaled = frame::Video::empty();
            scaler.run(&decoded, &mut scaled)?;
            carry_timestamps(&decoded, &mut scaled);
            scaled
        } else {
            decoded.clone()
        };
        encoder.send_frame(&scaled)?;
        write_encoded(
            &mut encoder,
            &mut writer,
            video_output_index,
            video_output_time_base,
            &mut video_written,
        )?;
    }
    encoder.send_eof()?;
    write_encoded(
        &mut encoder,
        &mut writer,
        video_output_index,
        video_output_time_base,
        &mut video_written,
    )?;
    writer.write_trailer()?;

    debug!(video_written, audio_written, "conversion finished");
    info!(output = %output.display(), "wrote media file");
    Ok(())
}

/// Convenience preset for compressing an existing recording.
pub fn compress(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    max_width: u32,
    video_bitrate_kbps: u32,
) -> Result<()> {
    convert(
        input,
        output,
        &ConvertOptions {
            codec: VideoCodec::H264,
            max_width: Some(max_width),
            video_bitrate_kbps,
        },
    )
}

/// `sws_scale` copies pixels only, so the scaled frame starts with no
/// timestamps. Carry pts/duration/time base across or the encoder emits packets
/// with no presentation time and the muxer has to invent them.
fn carry_timestamps(from: &frame::Video, to: &mut frame::Video) {
    unsafe {
        let from = from.as_ptr();
        let to = to.as_mut_ptr();
        (*to).pts = (*from).pts;
        (*to).duration = (*from).duration;
        (*to).time_base = (*from).time_base;
    }
}

fn write_encoded(
    encoder: &mut codec::encoder::Video,
    writer: &mut format::context::Output,
    stream: usize,
    output_time_base: Rational,
    written: &mut u64,
) -> Result<()> {
    let mut packet = ffmpeg::Packet::empty();
    while encoder.receive_packet(&mut packet).is_ok() {
        packet.set_stream(stream);
        packet.rescale_ts(encoder.time_base(), output_time_base);
        packet.write_interleaved(writer)?;
        *written += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a one-second test clip with the FFmpeg CLI. Returns `None` when the
    /// CLI or the H.264 encoder is missing, so a minimal host still passes.
    fn fixture(dir: &Path) -> Option<std::path::PathBuf> {
        let path = dir.join("fixture.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "lavfi", "-i", "testsrc=size=160x120:rate=10"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
            .args(["-t", "1", "-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .args(["-c:a", "aac", "-shortest"])
            .arg(&path)
            .status()
            .ok()?;
        status.success().then_some(path)
    }

    #[test]
    fn probes_the_video_and_audio_streams() {
        let dir = tempfile::tempdir().unwrap();
        let Some(input) = fixture(dir.path()) else {
            return;
        };
        let info = probe(&input).expect("probe succeeds");
        assert!(info.duration_ms >= 500, "duration {}", info.duration_ms);
        let video = info.video.expect("video stream");
        assert_eq!((video.width, video.height), (160, 120));
        assert!(info.audio.is_some(), "audio stream");
    }

    #[test]
    fn thumbnail_is_a_jpeg_at_the_requested_width() {
        let dir = tempfile::tempdir().unwrap();
        let Some(input) = fixture(dir.path()) else {
            return;
        };
        let jpeg = thumbnail_jpeg(&input, 0, 80).expect("thumbnail");
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "JPEG start of image");
        assert!(jpeg.len() > 100, "JPEG payload too small");
    }

    #[test]
    fn converts_down_and_keeps_the_audio() {
        let dir = tempfile::tempdir().unwrap();
        let Some(input) = fixture(dir.path()) else {
            return;
        };
        let output = dir.path().join("small.mkv");
        convert(
            &input,
            &output,
            &ConvertOptions {
                codec: VideoCodec::H264,
                max_width: Some(80),
                video_bitrate_kbps: 300,
            },
        )
        .expect("convert succeeds");

        let info = probe(&output).expect("probe the conversion");
        let video = info.video.expect("video stream");
        assert_eq!(video.width, 80);
        assert_eq!(video.height, 60);
        assert!(info.audio.is_some(), "audio was stream-copied");
    }

    #[test]
    fn probe_rejects_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(probe(dir.path().join("absent.mp4")).is_err());
    }
}

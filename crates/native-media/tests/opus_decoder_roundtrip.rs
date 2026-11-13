//! Regression: the FFmpeg native Opus decoder emits planar float (`FLTP`), not
//! interleaved S16. Reinterpreting those float bits as `i16` produces loud
//! noise, so a round trip must yield plausible PCM levels.

use sharescreen_native_media::decode::OpusDecoder;
use sharescreen_native_media::OpusEncoder;

const SAMPLE_RATE: f64 = 48_000.0;
const FREQUENCY: f64 = 440.0;
const AMPLITUDE: f64 = 0.5;
/// 20 ms at 48 kHz, stereo.
const FRAMES_PER_PACKET: usize = 960;
const CHANNELS: usize = 2;

fn sine_frame(phase: &mut f64) -> Vec<i16> {
    let step = 2.0 * std::f64::consts::PI * FREQUENCY / SAMPLE_RATE;
    let mut pcm = vec![0i16; FRAMES_PER_PACKET * CHANNELS];
    for frame in pcm.chunks_exact_mut(CHANNELS) {
        let sample = (*phase).sin() * AMPLITUDE * i16::MAX as f64;
        frame.fill(sample as i16);
        *phase += step;
        if *phase > std::f64::consts::TAU {
            *phase -= std::f64::consts::TAU;
        }
    }
    pcm
}

fn rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let energy: f64 = samples
        .iter()
        .map(|sample| (*sample as f64) * (*sample as f64))
        .sum();
    (energy / samples.len() as f64).sqrt()
}

#[test]
fn decoded_opus_packets_have_plausible_pcm_levels() {
    let mut encoder = OpusEncoder::new(96).expect("FFmpeg has an Opus encoder");
    let mut decoder = OpusDecoder::new().expect("FFmpeg has an Opus decoder");
    assert_eq!(
        encoder.frame_len(),
        FRAMES_PER_PACKET * CHANNELS,
        "encoder must produce 20 ms stereo frames"
    );

    let mut phase = 0.0;
    let mut decoded = Vec::new();
    let mut lengths = Vec::new();
    // One second of 20 ms packets.
    for _ in 0..50 {
        let pcm = sine_frame(&mut phase);
        let packet = encoder
            .encode(&pcm)
            .expect("encode packet")
            .expect("one packet per frame");
        let output = decoder.decode(&packet.data).expect("decode packet");
        lengths.push(output.len());
        decoded.extend_from_slice(&output);
    }

    // The decoder emits one 20 ms stereo frame per packet. A leading packet may
    // be shorter while the decoder primes, so only require the steady state.
    for length in &lengths[2..] {
        assert_eq!(
            *length,
            FRAMES_PER_PACKET * CHANNELS,
            "decoded packet has the wrong sample count"
        );
    }

    // Skip encoder/decoder priming, then measure the steady-state signal.
    let skip = FRAMES_PER_PACKET * CHANNELS * 3;
    let level = rms(&decoded[skip.min(decoded.len())..]);
    // A 0.5-amplitude sine has RMS ≈ 0.5 / sqrt(2) * 32767 ≈ 11585. Full-scale
    // noise from reading float bits as i16 is far above this band.
    assert!(
        (8_000.0..=14_000.0).contains(&level),
        "decoded PCM RMS {level:.0} is not a plausible sine level; \
         the decoder output is probably not converted to S16"
    );
}

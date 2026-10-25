//! `@dream/audio`: MP3, Ogg Vorbis and Ogg Opus decode to the length their encoder wrote,
//! lined up with the signal they were made from (so gapless trimming is right to the frame),
//! seek exactly (MP3, Vorbis) or converged after pre-roll (Opus), refuse what they are not, and
//! survive damaged files without taking the host down; and the same through Luau.
//!
//! The fixtures are one second of 440 Hz left and 660 Hz right at half scale, encoded by
//! ffmpeg with libmp3lame (128 kb/s, 44.1 kHz), libvorbis (q4, 44.1 kHz) and libopus (96 kb/s,
//! 48 kHz), plus a 5.1 Opus (mapping family 1) of the 440 Hz tone at six levels.

use l3i::Runtime;
use l3i::audio::{AudioExtension, DecodeErrorKind, Decoder, Format};
use l3i::convert::NewBuffer;
use l3i::extension::{RuntimePlan, RuntimePolicy};

const MP3: &[u8] = include_bytes!("fixtures/audio/tone.mp3");
const VORBIS: &[u8] = include_bytes!("fixtures/audio/tone.ogg");
const OPUS: &[u8] = include_bytes!("fixtures/audio/tone.opus");
const SURROUND: &[u8] = include_bytes!("fixtures/audio/surround.opus");

fn decode_all(decoder: &mut Decoder) -> Vec<f32> {
    let mut samples = Vec::new();
    let mut block = vec![0.0; 1000 * decoder.channels()];
    loop {
        let frames = decoder.read(&mut block).unwrap();
        if frames == 0 {
            return samples;
        }
        samples.extend_from_slice(&block[..frames * decoder.channels()]);
    }
}

/// The signal-to-error ratio of channel `channel` against `level * sin(2 pi frequency t)`
/// shifted by `lag` frames, in dB, over the middle of the second (a codec's onset and tail are
/// not the test).
fn alignment_at(samples: &[f32], channels: usize, channel: usize, tone: (f64, f64, f64), lag: f64) -> f64 {
    let (rate, frequency, level) = tone;
    let frames = samples.len() / channels;
    let (mut signal, mut error) = (0.0, 0.0);
    for frame in frames / 10..frames * 9 / 10 {
        let expected = level * (std::f64::consts::TAU * frequency * (frame as f64 + lag) / rate).sin();
        signal += expected * expected;
        error += (f64::from(samples[frame * channels + channel]) - expected).powi(2);
    }
    10.0 * (signal / error).log10()
}

fn alignment(samples: &[f32], channels: usize, channel: usize, rate: f64, frequency: f64, level: f64) -> f64 {
    alignment_at(samples, channels, channel, (rate, frequency, level), 0.0)
}

/// Whether channel `channel` matches the tone better unshifted than a frame either way: the
/// output starts on the signal's first frame, whatever the codec's own accuracy.
fn lined_up(samples: &[f32], channels: usize, channel: usize, tone: (f64, f64, f64)) -> bool {
    let centre = alignment_at(samples, channels, channel, tone, 0.0);
    centre > 20.0
        && [-1.0, 1.0].into_iter().all(|lag| alignment_at(samples, channels, channel, tone, lag) < centre - 2.0)
}

#[test]
fn formats_are_detected_from_their_first_bytes() {
    assert_eq!(Format::detect(MP3), Some(Format::Mp3));
    assert_eq!(Format::detect(VORBIS), Some(Format::Vorbis));
    assert_eq!(Format::detect(OPUS), Some(Format::Opus));
    assert_eq!(Format::detect(SURROUND), Some(Format::Opus));
    assert_eq!(Format::detect(b"RIFF\0\0\0\0WAVEfmt "), None);
    assert_eq!(Format::detect(b"OggS"), None);
    assert_eq!(Format::detect(&[]), None);
    let error = Decoder::new(b"not audio at all, just words".to_vec(), None).unwrap_err();
    assert_eq!(error.kind(), DecodeErrorKind::Unrecognized);
}

#[test]
fn each_format_decodes_to_its_encoded_length_lined_up_with_its_signal() {
    for (bytes, format, rate) in
        [(MP3, Format::Mp3, 44_100), (VORBIS, Format::Vorbis, 44_100), (OPUS, Format::Opus, 48_000)]
    {
        let mut decoder = Decoder::new(bytes.to_vec(), None).unwrap();
        assert_eq!((decoder.format(), decoder.rate(), decoder.channels()), (format, rate, 2));
        // One second exactly: the encoder delay and padding are gone, at both ends.
        assert_eq!(decoder.frames(), Some(u64::from(rate)), "{format}");
        let samples = decode_all(&mut decoder);
        assert_eq!(samples.len(), rate as usize * 2, "{format}");
        assert_eq!(decoder.position(), u64::from(rate));
        let rate = f64::from(rate);
        assert!(lined_up(&samples, 2, 0, (rate, 440.0, 0.5)), "{format}: left");
        assert!(lined_up(&samples, 2, 1, (rate, 660.0, 0.5)), "{format}: right");
        assert!(samples.iter().all(|sample| sample.is_finite() && sample.abs() < 1.0));
    }
}

#[test]
fn a_multistream_opus_decodes_every_channel_in_vorbis_order() {
    let mut decoder = Decoder::new(SURROUND.to_vec(), Some(Format::Opus)).unwrap();
    assert_eq!((decoder.rate(), decoder.channels(), decoder.frames()), (48_000, 6, Some(48_000)));
    let samples = decode_all(&mut decoder);
    assert_eq!(samples.len(), 48_000 * 6);
    // Vorbis order for six channels: front left, centre, front right, rear left, rear right, LFE.
    for (channel, level) in [(0, 0.5), (1, 0.3), (2, 0.4), (3, 0.2), (4, 0.1)] {
        let ratio = alignment(&samples, 6, channel, 48_000.0, 440.0, level);
        assert!(ratio > 20.0, "channel {channel}: {ratio:.1} dB");
    }
}

#[test]
fn seeking_lands_on_the_frame_a_straight_run_reaches() {
    for (bytes, exact) in [(MP3, true), (VORBIS, true), (OPUS, false)] {
        let mut decoder = Decoder::new(bytes.to_vec(), None).unwrap();
        let straight = decode_all(&mut decoder);
        for target in [0u64, 1, 1000, 12_345, 30_000, 44_099] {
            decoder.seek(target).unwrap();
            assert_eq!(decoder.position(), target);
            let mut block = vec![0.0; 2 * 512];
            let frames = decoder.read(&mut block).unwrap();
            let start = usize::try_from(target).unwrap() * 2;
            let expected = &straight[start..(start + frames * 2).min(straight.len())];
            assert_eq!(frames * 2, expected.len());
            let worst = block.iter().zip(expected).map(|(got, want)| (got - want).abs()).fold(0.0, f32::max);
            if exact {
                assert!(worst == 0.0, "{:?} at {target}: off by {worst}", decoder.format());
            } else {
                assert!(worst < 1e-4, "{:?} at {target}: off by {worst}", decoder.format());
            }
        }
        // Past the end is the end.
        decoder.seek(10_000_000).unwrap();
        assert_eq!(decoder.position(), decoder.frames().unwrap());
        assert_eq!(decoder.read(&mut [0.0; 8]).unwrap(), 0);
        decoder.seek(0).unwrap();
        assert_eq!(decode_all(&mut decoder).len(), straight.len());
    }
}

#[test]
fn a_read_takes_only_whole_frames() {
    let mut decoder = Decoder::new(VORBIS.to_vec(), None).unwrap();
    assert_eq!(decoder.read(&mut [0.0; 7]).unwrap(), 3);
    assert_eq!(decoder.read(&mut [0.0; 1]).unwrap(), 0);
    assert_eq!(decoder.position(), 3);
}

#[test]
fn the_wrong_format_and_broken_headers_are_refused() {
    let error = Decoder::new(VORBIS.to_vec(), Some(Format::Opus)).unwrap_err();
    assert_eq!(error.kind(), DecodeErrorKind::Malformed, "{error}");
    let error = Decoder::new(OPUS.to_vec(), Some(Format::Vorbis)).unwrap_err();
    assert_eq!(error.kind(), DecodeErrorKind::Malformed, "{error}");
    let error = Decoder::new(b"OggS but nothing after".to_vec(), Some(Format::Vorbis)).unwrap_err();
    assert_eq!(error.kind(), DecodeErrorKind::Malformed, "{error}");
    assert!(Decoder::new(VORBIS.to_vec(), Some(Format::Mp3)).is_err());
}

#[test]
fn a_file_cut_short_plays_up_to_the_cut() {
    for bytes in [MP3, VORBIS, OPUS] {
        // Three quarters: a Vorbis file's first kilobytes are its codebooks.
        let mut decoder = Decoder::new(bytes[..bytes.len() * 3 / 4].to_vec(), None).unwrap();
        // The cut may end the stream or be an error where it falls (half an MPEG frame can
        // read as a frame of another format); either way, what came before it plays.
        let mut frames = 0;
        let mut block = vec![0.0; 2 * 1000];
        while let Ok(read @ 1..) = decoder.read(&mut block) {
            frames += read as u64;
        }
        assert!(frames > 10_000 && frames < u64::from(decoder.rate()), "{:?}: {frames}", decoder.format());
    }
}

/// Damaged files: every outcome is a result, never a panic that escapes or a hang.
#[test]
fn damaged_files_fail_as_errors_or_decode() {
    let mut state = 0x2545_f491_u32;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    for bytes in [MP3, VORBIS, OPUS, SURROUND] {
        for _ in 0..40 {
            let mut damaged = bytes.to_vec();
            for _ in 0..=next() % 16 {
                let at = next() as usize % damaged.len();
                damaged[at] = next() as u8;
            }
            let Ok(mut decoder) = Decoder::new(damaged, None) else { continue };
            let mut block = vec![0.0; 4096 * decoder.channels()];
            for _ in 0..200 {
                match decoder.read(&mut block) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            let _ = decoder.seek(u64::from(next() % 50_000));
            let _ = decoder.read(&mut block);
        }
    }
}

fn runtime() -> Runtime {
    let plan = RuntimePlan::builder()
        .policy(RuntimePolicy::new().compat_global("@dream/audio", "audio"))
        .extension(AudioExtension)
        .finalize()
        .unwrap();
    let runtime = Runtime::from_plan(&plan).unwrap();
    runtime.set_global("mp3", &NewBuffer(MP3.to_vec())).unwrap();
    runtime.set_global("vorbis", &NewBuffer(VORBIS.to_vec())).unwrap();
    runtime.set_global("opus", &NewBuffer(OPUS.to_vec())).unwrap();
    runtime
}

#[test]
fn luau_decodes_into_buffers_it_owns() {
    runtime()
        .exec(
            "assert(audio.detect(mp3) == 'mp3' and audio.detect(vorbis) == 'vorbis' and audio.detect(opus) == 'opus') \
             assert(audio.detect('plain text') == nil) \
             assert(audio.detect(buffer.tostring(opus)) == 'opus', 'strings are bytes too') \
             local decoder = audio.decoder(vorbis) \
             assert(typeof(decoder) == 'dream.audio.Decoder') \
             assert(decoder:format() == 'vorbis' and decoder:rate() == 44100 and decoder:channels() == 2) \
             assert(decoder:frames() == 44100 and decoder:position() == 0) \
             local block = buffer.create(1000 * 8 + 4) \
             assert(decoder:read(block) == 1000, 'as many whole frames as fit') \
             assert(decoder:position() == 1000) \
             assert(decoder:read(block, 4, 10) == 10, 'a window of the buffer') \
             assert(buffer.readf32(block, 0) == buffer.readf32(block, 0), 'what came before is kept') \
             local total = 1010 \
             while true do \
               local frames = decoder:read(block) \
               if frames == 0 then break end \
               total += frames \
             end \
             assert(total == 44100, total) \
             decoder:seek(22050) \
             assert(decoder:position() == 22050) \
             local whole = audio.decode(mp3) \
             assert(whole.format == 'mp3' and whole.rate == 44100 and whole.channels == 2) \
             assert(whole.frames == 44100 and buffer.len(whole.data) == 44100 * 8) \
             local peak = 0 \
             for index = 0, whole.frames * 2 - 1 do \
               peak = math.max(peak, math.abs(buffer.readf32(whole.data, index * 4))) \
             end \
             assert(peak > 0.45 and peak < 0.6, peak) \
             assert(audio.decode(opus, { format = 'opus' }).frames == 48000)",
        )
        .unwrap();
}

#[test]
fn luau_errors_name_the_call() {
    runtime()
        .exec(
            "local function fails(needle, body) \
               local ok, err = pcall(body) \
               assert(not ok and tostring(err):find(needle, 1, true), tostring(err)) \
             end \
             fails('audio.decoder: not MP3, Ogg Vorbis or Ogg Opus', function() audio.decoder('words') end) \
             fails(\"format 'flac' is not mp3, vorbis or opus\", function() audio.decoder(mp3, { format = 'flac' }) end) \
             fails('audio.decoder: bad OpusHead', function() audio.decoder(vorbis, { format = 'opus' }) end) \
             fails('audio.decode: output exceeds maxSize (1000 bytes)', function() audio.decode(mp3, { maxSize = 1000 }) end) \
             local decoder = audio.decoder(opus) \
             fails('Decoder:read: offset -1 is negative', function() decoder:read(buffer.create(8), -1) end) \
             fails('Decoder:read: offset 9 past the end', function() decoder:read(buffer.create(8), 9) end) \
             fails('Decoder:read: 2 frames do not fit the buffer (room for 1 after offset 0)', \
               function() decoder:read(buffer.create(8), 0, 2) end) \
             fails('Decoder:seek: frame -5 is negative', function() decoder:seek(-5) end)",
        )
        .unwrap();
}

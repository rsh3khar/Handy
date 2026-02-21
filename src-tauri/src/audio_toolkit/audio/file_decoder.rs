use anyhow::{Context, Result};
use log::{debug, info};
use rubato::{FftFixedIn, Resampler};
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

const TARGET_SAMPLE_RATE: usize = 16_000;

/// Decode an audio file to mono f32 samples at 16kHz.
///
/// Supports WAV, MP3, FLAC, M4A/AAC, and OGG/Vorbis via symphonia.
pub fn decode_audio_file(path: &Path) -> Result<Vec<f32>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("Failed to open audio file: {}", path.display()))?;

    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    // Provide a hint based on file extension
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    // Probe the format
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .context("Failed to probe audio format")?;

    let mut format_reader = probed.format;

    // Find the first audio track
    let track = format_reader
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .context("No audio track found in file")?;

    let track_id = track.id;
    let codec_params = track.codec_params.clone();

    let source_sample_rate = codec_params
        .sample_rate
        .context("Audio track has no sample rate")? as usize;
    let channels = codec_params.channels.map(|c| c.count()).unwrap_or(1);

    debug!(
        "Audio file: {}Hz, {} channel(s)",
        source_sample_rate, channels
    );

    // Create a decoder for the track
    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .context("Failed to create audio decoder")?;

    // Decode all packets and collect interleaved samples
    let mut interleaved_samples: Vec<f32> = Vec::new();

    loop {
        let packet = match format_reader.next_packet() {
            Ok(packet) => packet,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break; // End of stream
            }
            Err(e) => return Err(e).context("Error reading audio packet"),
        };

        // Skip packets not belonging to our track
        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(symphonia::core::errors::Error::DecodeError(msg)) => {
                debug!("Decode error (skipping packet): {}", msg);
                continue;
            }
            Err(e) => return Err(e).context("Fatal decode error"),
        };

        let spec = *decoded.spec();
        let num_frames = decoded.frames();
        if num_frames == 0 {
            continue;
        }

        let mut sample_buf = SampleBuffer::<f32>::new(num_frames as u64, spec);
        sample_buf.copy_interleaved_ref(decoded);
        interleaved_samples.extend_from_slice(sample_buf.samples());
    }

    if interleaved_samples.is_empty() {
        anyhow::bail!("No audio samples decoded from file");
    }

    // Mix to mono if multi-channel
    let mono_samples = if channels > 1 {
        interleaved_samples
            .chunks_exact(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect()
    } else {
        interleaved_samples
    };

    // Resample to 16kHz if needed
    let final_samples = if source_sample_rate != TARGET_SAMPLE_RATE {
        resample(&mono_samples, source_sample_rate, TARGET_SAMPLE_RATE)?
    } else {
        mono_samples
    };

    let duration_secs = final_samples.len() as f64 / TARGET_SAMPLE_RATE as f64;
    info!(
        "Decoded audio: {:.1}s, {} samples at {}Hz",
        duration_secs,
        final_samples.len(),
        TARGET_SAMPLE_RATE
    );

    Ok(final_samples)
}

/// Resample audio from source to target sample rate using rubato.
fn resample(samples: &[f32], from_hz: usize, to_hz: usize) -> Result<Vec<f32>> {
    const CHUNK_SIZE: usize = 1024;

    let mut resampler = FftFixedIn::<f32>::new(from_hz, to_hz, CHUNK_SIZE, 1, 1)
        .context("Failed to create resampler")?;

    let mut output: Vec<f32> = Vec::with_capacity(
        (samples.len() as f64 * to_hz as f64 / from_hz as f64) as usize + CHUNK_SIZE,
    );

    // Process full chunks
    for chunk in samples.chunks(CHUNK_SIZE) {
        let input = if chunk.len() < CHUNK_SIZE {
            // Pad the last chunk with zeros
            let mut padded = chunk.to_vec();
            padded.resize(CHUNK_SIZE, 0.0);
            padded
        } else {
            chunk.to_vec()
        };

        let resampled = resampler
            .process(&[&input], None)
            .context("Resampling failed")?;
        output.extend_from_slice(&resampled[0]);
    }

    // Trim output to expected length (padding may have added extra)
    let expected_len = (samples.len() as f64 * to_hz as f64 / from_hz as f64).ceil() as usize;
    output.truncate(expected_len);

    Ok(output)
}

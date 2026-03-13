use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SizedSample;
use dasp::sample::ToSample;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[cfg(target_os = "macos")]
use cidre::{arc, av, cat, cf, core_audio as ca, ns, os};

const TAP_NAME: &str = "aside-tap";

/// +20 dB gain applied to mic input. Raw hardware levels from built-in macs
/// and USB mics (e.g. Yeti at moderate gain) typically sit around -50 to -40
/// dBFS RMS; this brings speech into the -30 to -20 dBFS range.
const MIC_GAIN: f32 = 10.0;

/// Pre-allocated ring buffer capacity: 10 seconds of audio at 48kHz.
/// Gives the consumer thread plenty of slack to drain without drops,
/// even if macOS timer coalescing stretches the drain interval.
const RING_BUF_SECONDS: usize = 10;

/// How often the consumer thread drains the ring buffer.
const DRAIN_INTERVAL: Duration = Duration::from_millis(5);

/// Threshold for detecting a dead Core Audio tap. A live tap with no audio still
/// produces noise-floor samples (~0.0002); a dead tap delivers exact 0.0 values.
/// Using a very small epsilon to account for floating-point artifacts.
const DEAD_TAP_THRESHOLD: f32 = 1e-7;

/// Handle to a running recorder. Call `stop_and_write()` to finalize.
pub struct RecorderHandle {
    stop_flag: Arc<AtomicBool>,
    mic_handle: std::thread::JoinHandle<Vec<f32>>,
    spk_handle: std::thread::JoinHandle<Vec<f32>>,
    // Keep streams alive until stop — drop order matters
    _mic_stream: cpal::Stream,
    _spk_capture: SpeakerCapture,
    sample_rate: u32,
    spk_rate: u32,
    mic_peak: Arc<AtomicU32>,
    spk_peak: Arc<AtomicU32>,
    mic_drops: Arc<AtomicU64>,
    spk_drops: Arc<AtomicU64>,
    /// Consecutive silent samples on the speaker channel. Reset on any
    /// non-silent sample. Used to detect Core Audio tap death.
    spk_silence: Arc<AtomicU64>,
}

impl RecorderHandle {
    /// Start capturing mic + system audio. Returns immediately.
    /// If `device` is `Some`, uses that mic; otherwise uses the system default.
    pub fn start(stop_flag: Arc<AtomicBool>, device: Option<&cpal::Device>) -> Result<Self> {
        let mic_peak = Arc::new(AtomicU32::new(0));
        let spk_peak = Arc::new(AtomicU32::new(0));
        let mic_drops = Arc::new(AtomicU64::new(0));
        let spk_drops = Arc::new(AtomicU64::new(0));
        let spk_silence = Arc::new(AtomicU64::new(0));

        let (mic_stream, mic_rate, mic_consumer) =
            start_mic(device, mic_peak.clone(), mic_drops.clone())?;
        let (spk_capture, spk_rate, spk_consumer) =
            start_speaker(spk_peak.clone(), spk_drops.clone(), spk_silence.clone())?;

        eprintln!(
            "Recording (mic {}Hz, speaker {}Hz)...",
            mic_rate, spk_rate
        );

        if mic_rate != spk_rate {
            eprintln!(
                "Warning: sample rates differ ({} vs {}). Using {}Hz for output.",
                mic_rate, spk_rate, mic_rate
            );
        }

        let mic_stop = stop_flag.clone();
        let mic_handle = std::thread::spawn(move || {
            drain_ring_buffer(mic_consumer, &mic_stop, mic_rate)
        });
        let spk_stop = stop_flag.clone();
        let spk_handle = std::thread::spawn(move || {
            drain_ring_buffer(spk_consumer, &spk_stop, spk_rate)
        });

        Ok(Self {
            stop_flag,
            mic_handle,
            spk_handle,
            _mic_stream: mic_stream,
            _spk_capture: spk_capture,
            sample_rate: mic_rate,
            spk_rate,
            mic_peak,
            spk_peak,
            mic_drops,
            spk_drops,
            spk_silence,
        })
    }

    pub fn mic_peak(&self) -> Arc<AtomicU32> {
        self.mic_peak.clone()
    }

    pub fn spk_peak(&self) -> Arc<AtomicU32> {
        self.spk_peak.clone()
    }

    pub fn mic_drops(&self) -> Arc<AtomicU64> {
        self.mic_drops.clone()
    }

    pub fn spk_drops(&self) -> Arc<AtomicU64> {
        self.spk_drops.clone()
    }

    /// Count of consecutive silent samples on the speaker channel.
    /// Divide by speaker sample rate to get seconds of silence.
    pub fn spk_silence(&self) -> Arc<AtomicU64> {
        self.spk_silence.clone()
    }

    pub fn spk_rate(&self) -> u32 {
        self.spk_rate
    }

    /// Stop recording and write WAV to `path`. Returns duration in seconds.
    pub fn stop_and_write(self, path: &str) -> Result<f64> {
        // Signal stop
        self.stop_flag.store(true, Ordering::SeqCst);

        // Drop streams to close channels
        drop(self._mic_stream);
        drop(self._spk_capture);

        let mic_samples = self.mic_handle.join().expect("mic collector panicked");
        let spk_samples = self.spk_handle.join().expect("spk collector panicked");

        let mic_dropped = self.mic_drops.load(Ordering::Relaxed);
        let spk_dropped = self.spk_drops.load(Ordering::Relaxed);
        if mic_dropped > 0 || spk_dropped > 0 {
            eprintln!(
                "Warning: dropped samples — mic: {}, speaker: {}",
                mic_dropped, spk_dropped
            );
        }

        if mic_samples.is_empty() && spk_samples.is_empty() {
            eprintln!("No audio captured.");
            return Ok(0.0);
        }

        let duration = write_stereo_wav(
            path,
            &mic_samples,
            &spk_samples,
            self.sample_rate,
            self.spk_rate,
        )?;
        eprintln!("Wrote {:.1}s stereo WAV to {}", duration, path);

        Ok(duration)
    }
}

/// Enumerate all available input devices. Returns (name, device) pairs.
pub fn list_input_devices() -> Vec<(String, cpal::Device)> {
    let host = cpal::default_host();
    let devices = match host.input_devices() {
        Ok(devs) => devs,
        Err(_) => return Vec::new(),
    };
    devices
        .filter_map(|d| {
            let name = d.name().ok()?;
            Some((name, d))
        })
        .collect()
}

/// Get the name of the default input device, if any.
pub fn default_input_device_name() -> Option<String> {
    let host = cpal::default_host();
    host.default_input_device().and_then(|d| d.name().ok())
}

// --- Resampling ---

/// Resample audio via linear interpolation. Good enough for speech destined
/// for whisper-cli, which resamples everything to 16kHz internally anyway.
fn resample(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || samples.is_empty() {
        return samples.to_vec();
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = (samples.len() as f64 / ratio).ceil() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 * ratio;
        let idx = src_pos as usize;
        let frac = (src_pos - idx as f64) as f32;
        let s = if idx + 1 < samples.len() {
            samples[idx] * (1.0 - frac) + samples[idx + 1] * frac
        } else if idx < samples.len() {
            samples[idx]
        } else {
            0.0
        };
        out.push(s);
    }
    out
}

// --- Shared consumer drain ---

/// Estimated max recording duration for pre-allocation (2 hours).
/// Avoids Vec reallocations during long recordings, which can stall
/// the drain thread long enough to overflow the ring buffer.
const PREALLOC_DURATION_SECS: usize = 7200;

/// Drain a ring buffer consumer into a Vec until the stop flag is set,
/// then do one final drain to pick up any remaining samples.
///
/// On macOS, elevates thread QoS to UserInteractive to prevent timer
/// coalescing from stretching the drain interval and causing overflows.
fn drain_ring_buffer(
    mut consumer: rtrb::Consumer<f32>,
    stop: &AtomicBool,
    sample_rate: u32,
) -> Vec<f32> {
    // Prevent macOS from deprioritizing this thread — timer coalescing
    // can stretch a 5ms sleep to 50ms+ under power saving, which risks
    // overflowing the ring buffer.
    #[cfg(target_os = "macos")]
    {
        use std::os::raw::c_int;
        extern "C" {
            fn pthread_set_qos_class_self_np(qos_class: c_int, relative_priority: c_int) -> c_int;
        }
        const QOS_CLASS_USER_INTERACTIVE: c_int = 0x21;
        unsafe {
            pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
        }
    }

    // Pre-allocate to avoid reallocations during recording. At 48kHz for
    // 2 hours this is ~345M samples (~1.4GB). The OS won't commit physical
    // pages until they're written, so this is cheap if the recording is shorter.
    let mut buf = Vec::with_capacity(sample_rate as usize * PREALLOC_DURATION_SECS);
    while !stop.load(Ordering::Relaxed) {
        while let Ok(sample) = consumer.pop() {
            buf.push(sample);
        }
        std::thread::sleep(DRAIN_INTERVAL);
    }
    // Final drain after stop — the audio callback may have pushed more samples
    // between our last pop and the stream being dropped.
    while let Ok(sample) = consumer.pop() {
        buf.push(sample);
    }
    buf
}

// --- Mic capture (cpal) ---

fn start_mic(
    device: Option<&cpal::Device>,
    peak: Arc<AtomicU32>,
    drops: Arc<AtomicU64>,
) -> Result<(cpal::Stream, u32, rtrb::Consumer<f32>)> {
    let host = cpal::default_host();
    let default_device;
    let device = match device {
        Some(d) => d,
        None => {
            default_device = host.default_input_device().context("no mic found")?;
            &default_device
        }
    };
    let config = device.default_input_config()?;
    let rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    let format = config.sample_format();

    let capacity = rate as usize * RING_BUF_SECONDS;
    let (producer, consumer) = rtrb::RingBuffer::new(capacity);

    let stream = match format {
        cpal::SampleFormat::F32 => build_mic_stream::<f32>(device, &config, producer, channels, peak, drops)?,
        cpal::SampleFormat::I16 => build_mic_stream::<i16>(device, &config, producer, channels, peak, drops)?,
        cpal::SampleFormat::I32 => build_mic_stream::<i32>(device, &config, producer, channels, peak, drops)?,
        _ => bail!("unsupported mic format: {:?}", format),
    };
    stream.play()?;

    Ok((stream, rate, consumer))
}

fn build_mic_stream<S: SizedSample + ToSample<f32> + Send + 'static>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    mut producer: rtrb::Producer<f32>,
    channels: usize,
    peak: Arc<AtomicU32>,
    drops: Arc<AtomicU64>,
) -> Result<cpal::Stream> {
    Ok(device.build_input_stream(
        &config.config(),
        move |data: &[S], _: &_| {
            for sample in data.iter().step_by(channels) {
                let s = (sample.to_sample::<f32>() * MIC_GAIN).clamp(-1.0, 1.0);
                peak.fetch_max(s.abs().to_bits(), Ordering::Relaxed);
                if producer.push(s).is_err() {
                    drops.fetch_add(1, Ordering::Relaxed);
                }
            }
        },
        |err| eprintln!("mic error: {err}"),
        None,
    )?)
}

// --- System audio capture (macOS Core Audio tap via cidre) ---

#[cfg(target_os = "macos")]
struct SpeakerCapture {
    _device: ca::hardware::StartedDevice<ca::AggregateDevice>,
    _ctx: Box<SpeakerCtx>,
    _tap: ca::TapGuard,
}

#[cfg(target_os = "macos")]
struct SpeakerCtx {
    producer: rtrb::Producer<f32>,
    format: arc::R<av::AudioFormat>,
    peak: Arc<AtomicU32>,
    drops: Arc<AtomicU64>,
    silence: Arc<AtomicU64>,
}

#[cfg(target_os = "macos")]
fn start_speaker(
    peak: Arc<AtomicU32>,
    drops: Arc<AtomicU64>,
    silence: Arc<AtomicU64>,
) -> Result<(SpeakerCapture, u32, rtrb::Consumer<f32>)> {
    use ca::aggregate_device_keys as agg_keys;

    let tap_desc = ca::TapDesc::with_mono_global_tap_excluding_processes(&ns::Array::new());
    let tap = tap_desc.create_process_tap()?;
    let asbd = tap.asbd()?;
    let rate = asbd.sample_rate as u32;
    let format = av::AudioFormat::with_asbd(&asbd).context("bad audio format from tap")?;

    let capacity = rate as usize * RING_BUF_SECONDS;
    let (producer, consumer) = rtrb::RingBuffer::new(capacity);

    let sub_tap = cf::DictionaryOf::with_keys_values(
        &[ca::sub_device_keys::uid()],
        &[tap.uid().context("no tap uid")?.as_type_ref()],
    );
    let agg_desc = cf::DictionaryOf::with_keys_values(
        &[
            agg_keys::is_private(),
            agg_keys::tap_auto_start(),
            agg_keys::name(),
            agg_keys::uid(),
            agg_keys::tap_list(),
        ],
        &[
            cf::Boolean::value_true().as_type_ref(),
            cf::Boolean::value_false(),
            cf::String::from_str(TAP_NAME).as_ref(),
            &cf::Uuid::new().to_cf_string(),
            &cf::ArrayOf::from_slice(&[sub_tap.as_ref()]),
        ],
    );

    let mut ctx = Box::new(SpeakerCtx { producer, format, peak, drops, silence });

    let agg_device = ca::AggregateDevice::with_desc(&agg_desc)
        .map_err(|e| anyhow::anyhow!("AggregateDevice::with_desc failed: {}", e))?;
    let proc_id = agg_device
        .create_io_proc_id(speaker_io_proc, Some(&mut ctx))
        .map_err(|e| anyhow::anyhow!("create_io_proc_id failed: {}", e))?;
    let device = ca::device_start(agg_device, Some(proc_id))
        .map_err(|e| anyhow::anyhow!("device_start failed: {}", e))?;

    Ok((
        SpeakerCapture {
            _device: device,
            _ctx: ctx,
            _tap: tap,
        },
        rate,
        consumer,
    ))
}

#[cfg(target_os = "macos")]
extern "C" fn speaker_io_proc(
    _device: ca::Device,
    _now: &cat::AudioTimeStamp,
    input_data: &cat::AudioBufList<1>,
    _input_time: &cat::AudioTimeStamp,
    _output_data: &mut cat::AudioBufList<1>,
    _output_time: &cat::AudioTimeStamp,
    ctx: Option<&mut SpeakerCtx>,
) -> os::Status {
    let ctx = ctx.unwrap();

    // Try typed PCM buffer first
    if let Some(view) = av::AudioPcmBuf::with_buf_list_no_copy(&ctx.format, input_data, None) {
        if let Some(data) = view.data_f32_at(0) {
            push_speaker_samples(ctx, data);
            return os::Status::NO_ERR;
        }
    }

    // Fallback: read raw f32 samples from buffer
    let buf = &input_data.buffers[0];
    if buf.data_bytes_size > 0 && !buf.data.is_null() {
        let count = buf.data_bytes_size as usize / std::mem::size_of::<f32>();
        if count > 0 {
            let data = unsafe { std::slice::from_raw_parts(buf.data as *const f32, count) };
            push_speaker_samples(ctx, data);
        }
    }

    os::Status::NO_ERR
}

/// Push speaker samples into the ring buffer, tracking drops and silence.
#[cfg(target_os = "macos")]
fn push_speaker_samples(ctx: &mut SpeakerCtx, data: &[f32]) {
    push_samples_tracked(
        &mut ctx.producer,
        &ctx.peak,
        &ctx.drops,
        Some(&ctx.silence),
        data,
    );
}

/// Core sample-push logic, platform-independent. Tracks peak level, dropped
/// samples (ring buffer full), and optionally consecutive dead-tap duration.
///
/// The silence counter detects a dead Core Audio tap by checking for exact-zero
/// samples (below `DEAD_TAP_THRESHOLD`). A live tap with nobody talking still
/// produces noise-floor samples that exceed this threshold.
fn push_samples_tracked(
    producer: &mut rtrb::Producer<f32>,
    peak: &AtomicU32,
    drops: &AtomicU64,
    silence: Option<&AtomicU64>,
    data: &[f32],
) {
    let mut all_dead = true;
    for &s in data {
        peak.fetch_max(s.abs().to_bits(), Ordering::Relaxed);
        if producer.push(s).is_err() {
            drops.fetch_add(1, Ordering::Relaxed);
        }
        if s.abs() > DEAD_TAP_THRESHOLD {
            all_dead = false;
        }
    }
    if let Some(silence) = silence {
        if all_dead {
            silence.fetch_add(data.len() as u64, Ordering::Relaxed);
        } else {
            silence.store(0, Ordering::Relaxed);
        }
    }
}

/// Write interleaved stereo WAV from two mono sample buffers.
/// Resamples `spk` to `mic_rate` if rates differ. Returns duration in seconds.
fn write_stereo_wav(
    path: &str,
    mic_samples: &[f32],
    spk_samples: &[f32],
    mic_rate: u32,
    spk_rate: u32,
) -> Result<f64> {
    let spk_resampled;
    let spk = if spk_rate != mic_rate {
        spk_resampled = resample(spk_samples, spk_rate, mic_rate);
        &spk_resampled
    } else {
        spk_samples
    };

    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: mic_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };

    let mut writer = hound::WavWriter::create(path, spec).context("failed to create WAV")?;

    let max_len = mic_samples.len().max(spk.len());
    for i in 0..max_len {
        let m = mic_samples.get(i).copied().unwrap_or(0.0);
        let s = spk.get(i).copied().unwrap_or(0.0);
        writer.write_sample(m)?;
        writer.write_sample(s)?;
    }
    writer.finalize()?;

    Ok(max_len as f64 / mic_rate as f64)
}

#[cfg(not(target_os = "macos"))]
struct SpeakerCapture;

#[cfg(not(target_os = "macos"))]
fn start_speaker(
    _peak: Arc<AtomicU32>,
    _drops: Arc<AtomicU64>,
    _silence: Arc<AtomicU64>,
) -> Result<(SpeakerCapture, u32, rtrb::Consumer<f32>)> {
    bail!("System audio capture is only supported on macOS")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::sync::Arc;

    // --- push_samples_tracked ---

    #[test]
    fn silence_detection_all_silent() {
        let (mut producer, _consumer) = rtrb::RingBuffer::new(1024);
        let peak = AtomicU32::new(0);
        let drops = AtomicU64::new(0);
        let silence = AtomicU64::new(0);

        let silent_data = vec![0.0f32; 480];
        push_samples_tracked(&mut producer, &peak, &drops, Some(&silence), &silent_data);

        assert_eq!(silence.load(Ordering::Relaxed), 480);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn silence_detection_resets_on_signal() {
        let (mut producer, _consumer) = rtrb::RingBuffer::new(4096);
        let peak = AtomicU32::new(0);
        let drops = AtomicU64::new(0);
        let silence = AtomicU64::new(0);

        // First: accumulate silence
        let silent = vec![0.0f32; 480];
        push_samples_tracked(&mut producer, &peak, &drops, Some(&silence), &silent);
        assert_eq!(silence.load(Ordering::Relaxed), 480);

        // More silence accumulates
        push_samples_tracked(&mut producer, &peak, &drops, Some(&silence), &silent);
        assert_eq!(silence.load(Ordering::Relaxed), 960);

        // Signal arrives — resets to 0
        let loud = vec![0.5f32; 100];
        push_samples_tracked(&mut producer, &peak, &drops, Some(&silence), &loud);
        assert_eq!(silence.load(Ordering::Relaxed), 0);

        // Silence starts counting from zero again
        push_samples_tracked(&mut producer, &peak, &drops, Some(&silence), &silent);
        assert_eq!(silence.load(Ordering::Relaxed), 480);
    }

    #[test]
    fn dead_tap_threshold_boundary() {
        let (mut producer, _consumer) = rtrb::RingBuffer::new(1024);
        let peak = AtomicU32::new(0);
        let drops = AtomicU64::new(0);
        let silence = AtomicU64::new(0);

        // Exactly at dead-tap threshold — should count as dead
        let at_threshold = vec![DEAD_TAP_THRESHOLD; 100];
        push_samples_tracked(&mut producer, &peak, &drops, Some(&silence), &at_threshold);
        assert_eq!(silence.load(Ordering::Relaxed), 100);

        // Just above dead-tap threshold — should reset (live noise floor)
        let above = vec![DEAD_TAP_THRESHOLD + 1e-6; 100];
        push_samples_tracked(&mut producer, &peak, &drops, Some(&silence), &above);
        assert_eq!(silence.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn noise_floor_does_not_trigger_dead_tap() {
        let (mut producer, _consumer) = rtrb::RingBuffer::new(4096);
        let peak = AtomicU32::new(0);
        let drops = AtomicU64::new(0);
        let silence = AtomicU64::new(0);

        // Typical Google Meet noise floor (~0.0002) — live tap, nobody talking
        let noise_floor = vec![0.0002f32; 480];
        push_samples_tracked(&mut producer, &peak, &drops, Some(&silence), &noise_floor);
        assert_eq!(silence.load(Ordering::Relaxed), 0, "noise floor should not count as dead tap");
    }

    #[test]
    fn drop_counting_when_buffer_full() {
        // Tiny buffer — will overflow immediately
        let (mut producer, _consumer) = rtrb::RingBuffer::new(10);
        let peak = AtomicU32::new(0);
        let drops = AtomicU64::new(0);

        let data = vec![0.5f32; 50];
        push_samples_tracked(&mut producer, &peak, &drops, None, &data);

        // 10 fit in the buffer, 40 dropped
        assert_eq!(drops.load(Ordering::Relaxed), 40);
    }

    #[test]
    fn peak_tracking() {
        let (mut producer, _consumer) = rtrb::RingBuffer::new(1024);
        let peak = AtomicU32::new(0);
        let drops = AtomicU64::new(0);

        let data = vec![0.1, 0.3, 0.7, 0.2, 0.05];
        push_samples_tracked(&mut producer, &peak, &drops, None, &data);

        let recorded_peak = f32::from_bits(peak.load(Ordering::Relaxed));
        assert!((recorded_peak - 0.7).abs() < 1e-6);
    }

    // --- drain_ring_buffer ---

    #[test]
    fn drain_collects_all_samples() {
        let (mut producer, consumer) = rtrb::RingBuffer::new(1024);
        let stop = Arc::new(AtomicBool::new(false));

        // Push some samples before starting drain
        for i in 0..100 {
            producer.push(i as f32 * 0.01).unwrap();
        }

        // Signal stop immediately — drain should still collect what's there
        stop.store(true, Ordering::SeqCst);
        let result = drain_ring_buffer(consumer, &stop, 48000);

        assert_eq!(result.len(), 100);
        assert!((result[0] - 0.0).abs() < 1e-6);
        assert!((result[99] - 0.99).abs() < 1e-6);
    }

    #[test]
    fn drain_preallocation() {
        let (_producer, consumer) = rtrb::RingBuffer::<f32>::new(64);
        let stop = Arc::new(AtomicBool::new(true)); // stop immediately

        let result = drain_ring_buffer(consumer, &stop, 48000);

        // Should have pre-allocated capacity even though it's empty
        assert!(result.capacity() >= 48000 * PREALLOC_DURATION_SECS);
        assert_eq!(result.len(), 0);
    }

    // --- resample ---

    #[test]
    fn resample_same_rate_is_identity() {
        let input = vec![1.0, 2.0, 3.0, 4.0];
        let output = resample(&input, 48000, 48000);
        assert_eq!(input, output);
    }

    #[test]
    fn resample_downsample_halves() {
        // 48kHz → 24kHz should roughly halve the sample count
        let input: Vec<f32> = (0..480).map(|i| (i as f32) / 480.0).collect();
        let output = resample(&input, 48000, 24000);
        assert_eq!(output.len(), 240);
        // First and last samples should be close to input endpoints
        assert!((output[0] - input[0]).abs() < 1e-4);
    }

    #[test]
    fn resample_upsample_doubles() {
        let input: Vec<f32> = (0..240).map(|i| (i as f32) / 240.0).collect();
        let output = resample(&input, 24000, 48000);
        assert_eq!(output.len(), 480);
    }

    #[test]
    fn resample_empty() {
        let output = resample(&[], 48000, 24000);
        assert!(output.is_empty());
    }

    // --- write_stereo_wav ---

    #[test]
    fn write_stereo_wav_basic() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_stereo.wav");
        let path_str = path.to_str().unwrap();

        let mic = vec![0.1f32; 480];
        let spk = vec![0.2f32; 480];

        let duration = write_stereo_wav(path_str, &mic, &spk, 48000, 48000).unwrap();
        assert!((duration - 0.01).abs() < 0.001); // 480/48000 = 0.01s

        // Read back and verify
        let reader = hound::WavReader::open(path_str).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.sample_rate, 48000);

        let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
        assert_eq!(samples.len(), 960); // 480 * 2 channels
        // Interleaved: mic, spk, mic, spk...
        assert!((samples[0] - 0.1).abs() < 1e-6); // ch0
        assert!((samples[1] - 0.2).abs() < 1e-6); // ch1

        std::fs::remove_file(path_str).ok();
    }

    #[test]
    fn write_stereo_wav_with_resample() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_stereo_resample.wav");
        let path_str = path.to_str().unwrap();

        let mic = vec![0.1f32; 240]; // 240 samples at 24kHz = 0.01s
        let spk = vec![0.2f32; 480]; // 480 samples at 48kHz = 0.01s

        let duration = write_stereo_wav(path_str, &mic, &spk, 24000, 48000).unwrap();
        assert!((duration - 0.01).abs() < 0.001);

        let reader = hound::WavReader::open(path_str).unwrap();
        assert_eq!(reader.spec().sample_rate, 24000);
        assert_eq!(reader.spec().channels, 2);

        std::fs::remove_file(path_str).ok();
    }

    #[test]
    fn write_stereo_wav_unequal_lengths_pads_zeros() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_stereo_unequal.wav");
        let path_str = path.to_str().unwrap();

        let mic = vec![0.5f32; 100];
        let spk = vec![0.3f32; 50]; // shorter

        write_stereo_wav(path_str, &mic, &spk, 48000, 48000).unwrap();

        let reader = hound::WavReader::open(path_str).unwrap();
        let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
        assert_eq!(samples.len(), 200); // 100 * 2 channels (padded to longer)

        // Past spk's length, ch1 should be 0.0
        assert!((samples[198] - 0.5).abs() < 1e-6); // mic at index 99
        assert!((samples[199] - 0.0).abs() < 1e-6); // spk padded with 0

        std::fs::remove_file(path_str).ok();
    }

    // --- Integrated: ring buffer pressure simulation ---

    #[test]
    fn simulated_buffer_pressure() {
        // Simulates a producer pushing faster than consumer drains,
        // verifying drop counting works under contention.
        let (mut producer, consumer) = rtrb::RingBuffer::new(100);
        let stop = Arc::new(AtomicBool::new(false));
        let peak = AtomicU32::new(0);

        // Fill buffer to 80%
        for _ in 0..80 {
            producer.push(0.1).unwrap();
        }

        // Now push 50 more with drop tracking — 30 fit, 20 drop
        let data = vec![0.1f32; 50];
        let drops_local = AtomicU64::new(0);
        push_samples_tracked(&mut producer, &peak, &drops_local, None, &data);
        // Ring buffer has capacity 100, 80 already in, ~20 fit (rtrb uses
        // one slot as sentinel, so actual usable capacity is 99)
        let dropped = drops_local.load(Ordering::Relaxed);
        assert!(dropped > 0, "expected some drops, got 0");
        assert!(dropped <= 50, "can't drop more than we pushed");

        // Drain should recover all non-dropped samples
        stop.store(true, Ordering::SeqCst);
        let drained = drain_ring_buffer(consumer, &stop, 48000);
        assert_eq!(drained.len() as u64 + dropped, 130); // 80 + 50 = 130 total
    }

    // --- Tap restart threshold ---

    #[test]
    fn tap_restart_threshold_calculation() {
        // Verify the silence-to-seconds conversion matches what the TUI uses
        let spk_rate: u32 = 48000;
        let threshold_secs: u64 = 10;
        let threshold_samples = threshold_secs * spk_rate as u64;

        let silence = AtomicU64::new(threshold_samples - 1);
        let silent_secs = silence.load(Ordering::Relaxed) / spk_rate as u64;
        assert_eq!(silent_secs, 9); // not yet at threshold

        silence.store(threshold_samples, Ordering::Relaxed);
        let silent_secs = silence.load(Ordering::Relaxed) / spk_rate as u64;
        assert_eq!(silent_secs, 10); // at threshold — should trigger restart
    }
}

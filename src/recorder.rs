use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SizedSample;
use dasp::sample::ToSample;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

#[cfg(target_os = "macos")]
use cidre::{arc, av, cat, cf, core_audio as ca, ns, os};

const TAP_NAME: &str = "aside-tap";

/// +20 dB gain applied to mic input. Raw hardware levels from built-in macs
/// and USB mics (e.g. Yeti at moderate gain) typically sit around -50 to -40
/// dBFS RMS; this brings speech into the -30 to -20 dBFS range.
const MIC_GAIN: f32 = 10.0;

/// Handle to a running recorder. Call `stop_and_write()` to finalize.
pub struct RecorderHandle {
    stop_flag: Arc<AtomicBool>,
    mic_handle: std::thread::JoinHandle<Vec<f32>>,
    spk_handle: std::thread::JoinHandle<Vec<f32>>,
    // Keep streams alive until stop — drop order matters
    _mic_stream: cpal::Stream,
    _spk_capture: SpeakerCapture,
    sample_rate: u32,
}

impl RecorderHandle {
    /// Start capturing mic + system audio. Returns immediately.
    /// If `device` is `Some`, uses that mic; otherwise uses the system default.
    pub fn start(stop_flag: Arc<AtomicBool>, device: Option<&cpal::Device>) -> Result<Self> {
        let (mic_tx, mic_rx) = mpsc::channel::<Vec<f32>>();
        let (spk_tx, spk_rx) = mpsc::channel::<Vec<f32>>();

        let (mic_stream, mic_rate) = start_mic(mic_tx, device)?;
        let (spk_capture, spk_rate) = start_speaker(spk_tx)?;

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

        let mic_handle = std::thread::spawn(move || {
            let mut buf = Vec::new();
            while let Ok(chunk) = mic_rx.recv() {
                buf.extend(chunk);
            }
            buf
        });
        let spk_handle = std::thread::spawn(move || {
            let mut buf = Vec::new();
            while let Ok(chunk) = spk_rx.recv() {
                buf.extend(chunk);
            }
            buf
        });

        Ok(Self {
            stop_flag,
            mic_handle,
            spk_handle,
            _mic_stream: mic_stream,
            _spk_capture: spk_capture,
            sample_rate: mic_rate,
        })
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

        if mic_samples.is_empty() && spk_samples.is_empty() {
            eprintln!("No audio captured.");
            return Ok(0.0);
        }

        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: self.sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };

        let mut writer =
            hound::WavWriter::create(path, spec).context("failed to create WAV")?;

        let max_len = mic_samples.len().max(spk_samples.len());
        for i in 0..max_len {
            let m = mic_samples.get(i).copied().unwrap_or(0.0);
            let s = spk_samples.get(i).copied().unwrap_or(0.0);
            writer.write_sample(m)?;
            writer.write_sample(s)?;
        }
        writer.finalize()?;

        let duration = max_len as f64 / self.sample_rate as f64;
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

// --- Mic capture (cpal) ---

fn start_mic(tx: mpsc::Sender<Vec<f32>>, device: Option<&cpal::Device>) -> Result<(cpal::Stream, u32)> {
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

    let stream = match format {
        cpal::SampleFormat::F32 => build_mic_stream::<f32>(device, &config, tx, channels)?,
        cpal::SampleFormat::I16 => build_mic_stream::<i16>(device, &config, tx, channels)?,
        cpal::SampleFormat::I32 => build_mic_stream::<i32>(device, &config, tx, channels)?,
        _ => bail!("unsupported mic format: {:?}", format),
    };
    stream.play()?;

    Ok((stream, rate))
}

fn build_mic_stream<S: SizedSample + ToSample<f32> + Send + 'static>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    tx: mpsc::Sender<Vec<f32>>,
    channels: usize,
) -> Result<cpal::Stream> {
    Ok(device.build_input_stream(
        &config.config(),
        move |data: &[S], _: &_| {
            let mono: Vec<f32> = data
                .iter()
                .step_by(channels)
                .map(|&s| (s.to_sample::<f32>() * MIC_GAIN).clamp(-1.0, 1.0))
                .collect();
            let _ = tx.send(mono);
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
    tx: mpsc::Sender<Vec<f32>>,
    format: arc::R<av::AudioFormat>,
}

#[cfg(target_os = "macos")]
fn start_speaker(tx: mpsc::Sender<Vec<f32>>) -> Result<(SpeakerCapture, u32)> {
    use ca::aggregate_device_keys as agg_keys;

    let tap_desc = ca::TapDesc::with_mono_global_tap_excluding_processes(&ns::Array::new());
    let tap = tap_desc.create_process_tap()?;
    let asbd = tap.asbd()?;
    let rate = asbd.sample_rate as u32;
    let format = av::AudioFormat::with_asbd(&asbd).context("bad audio format from tap")?;

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

    let mut ctx = Box::new(SpeakerCtx { tx, format });

    let agg_device = ca::AggregateDevice::with_desc(&agg_desc)?;
    let proc_id = agg_device.create_io_proc_id(speaker_io_proc, Some(&mut ctx))?;
    let device = ca::device_start(agg_device, Some(proc_id))?;

    Ok((
        SpeakerCapture {
            _device: device,
            _ctx: ctx,
            _tap: tap,
        },
        rate,
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
            let _ = ctx.tx.send(data.to_vec());
            return os::Status::NO_ERR;
        }
    }

    // Fallback: read raw f32 samples from buffer
    let buf = &input_data.buffers[0];
    if buf.data_bytes_size > 0 && !buf.data.is_null() {
        let count = buf.data_bytes_size as usize / std::mem::size_of::<f32>();
        if count > 0 {
            let data = unsafe { std::slice::from_raw_parts(buf.data as *const f32, count) };
            let _ = ctx.tx.send(data.to_vec());
        }
    }

    os::Status::NO_ERR
}

#[cfg(not(target_os = "macos"))]
struct SpeakerCapture;

#[cfg(not(target_os = "macos"))]
fn start_speaker(_tx: mpsc::Sender<Vec<f32>>) -> Result<(SpeakerCapture, u32)> {
    bail!("System audio capture is only supported on macOS")
}

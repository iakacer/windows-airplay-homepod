//! Core casting logic: discovery, an AirPlay streaming session, and WASAPI
//! loopback capture. Kept independent of any UI so it can be driven from the
//! CLI (`--list`) or the system-tray control thread.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use airplay_audio::{AlacEncoder, LiveAudioDecoder, LiveFrameSender, LivePcmFrame};
use airplay_client::Connection;
use airplay_core::device::Device;
use airplay_core::stream::{PtpMode, StreamType, TimingProtocol};
use airplay_core::{AudioCodec, AudioFormat, StreamConfig};
use airplay_discovery::{Discovery, ServiceBrowser};
use wasapi::{initialize_mta, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

/// Capture/stream sample rate and layout. WASAPI autoconverts the system mix to
/// this, so the live decoder receives it as-is (no resampling on our side).
const RATE: u32 = 44_100;
const CHANNELS: u8 = 2;

/// HomePod volume (0.0–1.0) used on first connect, kept low so it doesn't blast.
pub const DEFAULT_VOLUME: f32 = 0.25;

fn volume_file() -> Option<std::path::PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(std::path::Path::new(&appdata).join("HomePodCast").join("volume.txt"))
}

/// Load the last-used volume (0.0–1.0), or [`DEFAULT_VOLUME`] if none saved.
pub fn load_volume() -> f32 {
    volume_file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse::<f32>().ok())
        .map(|v| v.clamp(0.0, 1.0))
        .unwrap_or(DEFAULT_VOLUME)
}

/// Persist the chosen volume so it's remembered across runs.
pub fn save_volume(v: f32) {
    if let Some(p) = volume_file() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, format!("{:.3}", v.clamp(0.0, 1.0)));
    }
}

/// Discover AirPlay 2 audio devices on the LAN, HomePods first.
pub async fn discover(timeout: Duration) -> anyhow::Result<Vec<Device>> {
    let browser = ServiceBrowser::new()?;
    let mut devices = browser.scan(timeout).await?;
    devices.retain(|d| d.supports_airplay2() && d.addresses.iter().any(|a| a.is_ipv4()));
    devices.sort_by_key(|d| u8::from(!d.model.starts_with("AudioAccessory")));
    Ok(devices)
}

/// An active stream to one device: the AirPlay connection plus the background
/// WASAPI loopback capture thread feeding it.
pub struct Session {
    conn: Connection,
    cap_stop: Arc<AtomicBool>,
    cap_handle: Option<JoinHandle<()>>,
}

impl Session {
    /// Connect (AirPlay 2 transient pairing), set up the stream, and start
    /// pushing live system audio to the device.
    pub async fn start(mut device: Device, volume: f32) -> anyhow::Result<Self> {
        let ipv4 = device
            .addresses
            .iter()
            .find(|a| a.is_ipv4())
            .copied()
            .ok_or_else(|| anyhow::anyhow!("device has no IPv4 address"))?;
        device.addresses = vec![ipv4];

        // Same config as the validated file-streaming path: ALAC + NTP timing.
        let audio_format = AudioFormat::default();
        let asc = if audio_format.codec == AudioCodec::Alac {
            Some(AlacEncoder::new(audio_format.clone())?.magic_cookie())
        } else {
            None
        };
        let config = StreamConfig {
            stream_type: StreamType::Realtime,
            audio_format,
            timing_protocol: TimingProtocol::Ntp,
            ptp_mode: PtpMode::Master,
            latency_min: 22050,
            latency_max: 88200,
            supports_dynamic_stream_id: true,
            asc,
        };

        let mut conn = Connection::connect_auto(device, config, "3939").await?;
        conn.setup().await?;

        // Set volume BEFORE audio starts so the first packets aren't at the
        // library's default of 1.0 (full blast). start_streaming_live re-applies
        // this same value internally.
        let _ = conn.set_volume(volume).await;

        // Start capture BEFORE start_streaming_live so the buffer pre-fills (with
        // real audio or silence) — otherwise the streamer's buffer-fill wait times
        // out at 0% and starts with underruns.
        let (sender, decoder) = LiveAudioDecoder::create_pair(RATE, CHANNELS, 32);
        let cap_stop = Arc::new(AtomicBool::new(false));
        let stop2 = cap_stop.clone();
        let cap_handle = std::thread::spawn(move || {
            if let Err(e) = run_capture(sender, stop2) {
                tracing::error!("capture error: {e:#}");
            }
        });

        if let Err(e) = conn.start_streaming_live(decoder).await {
            cap_stop.store(true, Ordering::Relaxed);
            let _ = cap_handle.join();
            return Err(e.into());
        }

        Ok(Self {
            conn,
            cap_stop,
            cap_handle: Some(cap_handle),
        })
    }

    /// Send the periodic AirPlay 2 keepalive. The receiver expects this every
    /// ~2 s; without it the HomePod tears the session down and audio stops.
    pub async fn feedback(&mut self) {
        let _ = self.conn.send_feedback().await;
    }

    /// Change the playback volume (0.0–1.0) on the connected device.
    pub async fn set_volume(&mut self, volume: f32) {
        let _ = self.conn.set_volume(volume).await;
    }

    /// Stop capture and tear down the AirPlay connection.
    pub async fn stop(mut self) {
        // Stop capture first: dropping the sender disconnects the live decoder,
        // so the streamer winds down.
        self.cap_stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.cap_handle.take() {
            let _ = h.join();
        }
        // Tear down the connection, but bound it with a timeout: this alpha stack
        // can block on teardown, and hanging here would freeze the next Start.
        let _ = tokio::time::timeout(Duration::from_secs(4), self.conn.disconnect()).await;
    }
}

/// Capture the default render endpoint via WASAPI loopback and push PCM frames
/// to the AirPlay live decoder until `stop` is set.
fn run_capture(sender: LiveFrameSender, stop: Arc<AtomicBool>) -> anyhow::Result<()> {
    // Keep the COM guard alive for the lifetime of this thread.
    let _com = initialize_mta();

    let enumerator = DeviceEnumerator::new()?;
    let device = enumerator.get_default_device(&Direction::Render)?;
    let mut audio_client = device.get_iaudioclient()?;

    // Render device + Capture direction + Shared mode => loopback. autoconvert
    // gives us 44.1 kHz / stereo / f32 regardless of the device's mix format.
    let format = WaveFormat::new(32, 32, &SampleType::Float, RATE as usize, CHANNELS as usize, None);
    let (_default_period, min_period) = audio_client.get_periods()?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: min_period,
    };
    audio_client.initialize_client(&format, &Direction::Capture, &mode)?;

    let h_event = audio_client.set_get_eventhandle()?;
    let capture_client = audio_client.get_audiocaptureclient()?;
    let mut queue: VecDeque<u8> = VecDeque::new();
    audio_client.start_stream()?;
    tracing::info!("loopback capture started ({RATE} Hz, {CHANNELS}ch, f32)");

    let bytes_per_frame = CHANNELS as usize * 4; // f32 per sample
    // ~50 ms of silence used to keep the receiver primed while the PC is idle
    // (WASAPI loopback delivers no data when nothing is playing).
    let silence = vec![0i16; (RATE as usize / 20) * CHANNELS as usize];

    while !stop.load(Ordering::Relaxed) {
        capture_client.read_from_device_to_deque(&mut queue)?;

        let n_frames = queue.len() / bytes_per_frame;
        if n_frames > 0 {
            let n_samples = n_frames * CHANNELS as usize;
            let mut samples: Vec<i16> = Vec::with_capacity(n_samples);
            for _ in 0..n_samples {
                let b = [
                    queue.pop_front().unwrap(),
                    queue.pop_front().unwrap(),
                    queue.pop_front().unwrap(),
                    queue.pop_front().unwrap(),
                ];
                let f = f32::from_le_bytes(b);
                samples.push((f.clamp(-1.0, 1.0) * 32767.0) as i16);
            }
            sender.try_send(LivePcmFrame {
                samples,
                channels: CHANNELS,
                sample_rate: RATE,
            });
        } else {
            // Idle: push silence so the receiver's buffer never starves and drops
            // the stream (which would prevent audio from resuming).
            sender.try_send(LivePcmFrame {
                samples: silence.clone(),
                channels: CHANNELS,
                sample_rate: RATE,
            });
        }

        // Responsive (~event latency) while audio plays; ~50 ms idle polling to
        // pace the silence keepalive.
        let _ = h_event.wait_for_event(50);
    }

    let _ = audio_client.stop_stream();
    Ok(())
}

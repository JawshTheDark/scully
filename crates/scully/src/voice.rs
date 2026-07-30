//! Native voice calls over a self-hosted LiveKit SFU.
//!
//! This is the media engine, and it is deliberately NOT a webview: `livekit`
//! links libwebrtc (capture-side codecs, ICE, SRTP, jitter buffer) as a native
//! library, exactly the way GTK is a native library. The only thing libwebrtc's
//! Rust SDK leaves to the embedder is the actual OS audio I/O — the microphone
//! and the speakers — which we drive with `cpal`.
//!
//! Threading. Three worlds meet here and must not block each other:
//!   * cpal callback threads — real-time, must never allocate/lock for long.
//!   * the tokio runtime — where the LiveKit `Room` lives and its events arrive.
//!   * the glib main loop — where the call window lives.
//! The bridge: cpal's input callback pushes raw samples down an unbounded mpsc
//! to a tokio task that feeds LiveKit; each remote track's decoded frames land
//! in a per-track queue that cpal's output callback mixes; and `Room` events are
//! translated to [`CallEvent`]s on an `async_channel` the window drains on glib.
//!
//! Scope (voice-only MVP, per the agreed plan): one published mic track, N
//! subscribed remote audio tracks, mute, and hangup. Video is a later phase.
//!
//! Caveat worth stating plainly: the sample path assumes the input and output
//! devices' native rates (libwebrtc resamples internally to/from the Opus
//! encoder, so we create the source/streams AT the device rate and never
//! resample ourselves). It handles f32 and i16 device formats — the two PipeWire
//! and PulseAudio actually hand out — and errors clearly on anything else.
#![cfg(feature = "voice")]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use futures_util::StreamExt;

use livekit::options::TrackPublishOptions;
use livekit::track::{LocalAudioTrack, LocalTrack, RemoteTrack, TrackSource};
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::{Room, RoomEvent, RoomOptions};

/// What the call window needs to know, in its own vocabulary — never a raw
/// LiveKit type, so the UI layer stays free of the SDK.
#[derive(Clone, Debug)]
pub enum CallEvent {
    /// The room is joined; media is flowing.
    Connected,
    /// A remote participant joined (their identity — our IRC nick convention).
    ParticipantJoined(String),
    /// A remote participant left.
    ParticipantLeft(String),
    /// The current set of participants detected as speaking (identities).
    Speaking(Vec<String>),
    /// The call ended (server-side, network loss, or our own hangup).
    Ended(String),
    /// A fatal setup error (no mic, unsupported device format, connect failed).
    Failed(String),
}

/// A per-remote-track queue of interleaved i16 samples at the OUTPUT device's
/// rate/channels. The output callback pops from every live queue and sums.
type TrackQueue = Arc<Mutex<VecDeque<i16>>>;

/// The shared playback mixer: the set of currently-subscribed remote tracks.
/// Cloned into the output callback (to read) and the event task (to add/remove).
#[derive(Clone, Default)]
struct Mixer(Arc<Mutex<Vec<(String, TrackQueue)>>>);

impl Mixer {
    fn add(&self, id: String) -> TrackQueue {
        let q: TrackQueue = Arc::new(Mutex::new(VecDeque::new()));
        self.0.lock().unwrap().push((id, q.clone()));
        q
    }
    fn remove(&self, id: &str) {
        self.0.lock().unwrap().retain(|(k, _)| k != id);
    }
    /// Pop and sum one sample across all live tracks (saturating).
    fn next_sample(&self) -> i16 {
        let tracks = self.0.lock().unwrap();
        let mut acc: i32 = 0;
        for (_, q) in tracks.iter() {
            if let Some(s) = q.lock().unwrap().pop_front() {
                acc += s as i32;
            }
        }
        acc.clamp(i16::MIN as i32, i16::MAX as i32) as i16
    }
}

/// A live call. Dropping it stops the microphone and speakers immediately and
/// asks the room to close; hold it for as long as the call window is open.
pub struct Call {
    muted: Arc<AtomicBool>,
    close: Arc<tokio::sync::Notify>,
    /// cpal streams: kept alive purely so their audio callbacks keep running.
    _input: Stream,
    _output: Stream,
    /// Drained on the glib main loop by the call window.
    pub events: async_channel::Receiver<CallEvent>,
}

impl Call {
    /// Mute or unmute the microphone. Muting keeps publishing — it feeds silence
    /// — so the remote side sees us as present-but-quiet rather than gone.
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    /// End the call. The room closes on the tokio side; audio has already
    /// stopped because the streams drop with `self`.
    pub fn hangup(&self) {
        self.close.notify_waiters();
    }

    /// Start a call: open the audio devices, then connect + publish on the tokio
    /// runtime. Returns immediately; progress arrives as [`CallEvent`]s. `rt` is
    /// scully's shared tokio runtime handle; `url`/`token` come from
    /// `Rest::voice_token`.
    ///
    /// Errors here are only the synchronous ones (no audio device, unsupported
    /// format) — connect/publish failures arrive later as [`CallEvent::Failed`].
    pub fn start(
        rt: tokio::runtime::Handle,
        url: String,
        token: String,
    ) -> Result<Call, String> {
        let host = cpal::default_host();

        // ── Output device first: the playback mixer + speaker stream. We can
        // start it before connecting; it plays silence until tracks arrive.
        let out_dev = host
            .default_output_device()
            .ok_or_else(|| "no audio output device".to_string())?;
        let out_cfg = out_dev
            .default_output_config()
            .map_err(|e| format!("output config: {e}"))?;
        let out_rate = out_cfg.sample_rate().0;
        let out_channels = out_cfg.config().channels as u32;
        let mixer = Mixer::default();
        let output = build_output(&out_dev, &out_cfg, mixer.clone())?;
        output.play().map_err(|e| format!("play output: {e}"))?;

        // ── Input device: mic stream feeds an unbounded mpsc that the tokio
        // capture task drains into LiveKit's NativeAudioSource.
        let in_dev = host
            .default_input_device()
            .ok_or_else(|| "no microphone".to_string())?;
        let in_cfg = in_dev
            .default_input_config()
            .map_err(|e| format!("input config: {e}"))?;
        let in_rate = in_cfg.sample_rate().0;
        let in_channels = in_cfg.config().channels as u32;
        let (cap_tx, cap_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<i16>>();
        let input = build_input(&in_dev, &in_cfg, cap_tx)?;
        input.play().map_err(|e| format!("play input: {e}"))?;

        let muted = Arc::new(AtomicBool::new(false));
        let close = Arc::new(tokio::sync::Notify::new());
        let (ev_tx, ev_rx) = async_channel::unbounded::<CallEvent>();

        // Everything the tokio side owns.
        let task = RoomTask {
            url,
            token,
            in_rate,
            in_channels,
            out_rate,
            out_channels,
            muted: muted.clone(),
            close: close.clone(),
            mixer,
            events: ev_tx,
        };
        rt.spawn(task.run(cap_rx));

        Ok(Call { muted, close, _input: input, _output: output, events: ev_rx })
    }
}

/// The tokio-side half: owns the `Room` and everything async.
struct RoomTask {
    url: String,
    token: String,
    in_rate: u32,
    in_channels: u32,
    out_rate: u32,
    out_channels: u32,
    muted: Arc<AtomicBool>,
    close: Arc<tokio::sync::Notify>,
    mixer: Mixer,
    events: async_channel::Sender<CallEvent>,
}

impl RoomTask {
    async fn run(self, cap_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<i16>>) {
        let emit = |e: CallEvent| {
            let _ = self.events.try_send(e);
        };

        // Connect.
        let (room, mut room_events) =
            match Room::connect(&self.url, &self.token, RoomOptions::default()).await {
                Ok(pair) => pair,
                Err(e) => {
                    emit(CallEvent::Failed(format!("connect: {e}")));
                    return;
                }
            };

        // Publish the microphone. The source is created at the mic's native rate
        // so we never resample; libwebrtc resamples to the Opus encoder itself.
        let source = NativeAudioSource::new(
            AudioSourceOptions {
                echo_cancellation: true,
                noise_suppression: true,
                auto_gain_control: true,
            },
            self.in_rate,
            self.in_channels,
            1000, // internal queue depth (ms); must be a multiple of 10
        );
        let track = LocalAudioTrack::create_audio_track(
            "mic",
            RtcAudioSource::Native(source.clone()),
        );
        if let Err(e) = room
            .local_participant()
            .publish_track(
                LocalTrack::Audio(track),
                TrackPublishOptions {
                    source: TrackSource::Microphone,
                    ..Default::default()
                },
            )
            .await
        {
            emit(CallEvent::Failed(format!("publish mic: {e}")));
            let _ = room.close().await;
            return;
        }

        // Mic pump: cpal samples → 10ms frames → source.capture_frame.
        tokio::spawn(capture_pump(
            cap_rx,
            source,
            self.in_rate,
            self.in_channels,
            self.muted.clone(),
        ));

        emit(CallEvent::Connected);

        // Event loop: translate RoomEvents, wire playback, honour hangup.
        loop {
            tokio::select! {
                _ = self.close.notified() => {
                    let _ = room.close().await;
                    emit(CallEvent::Ended("you left the call".into()));
                    return;
                }
                ev = room_events.recv() => {
                    let Some(ev) = ev else {
                        emit(CallEvent::Ended("disconnected".into()));
                        return;
                    };
                    self.handle_room_event(ev, &emit);
                }
            }
        }
    }

    fn handle_room_event(&self, ev: RoomEvent, emit: &impl Fn(CallEvent)) {
        match ev {
            RoomEvent::ParticipantConnected(p) => {
                emit(CallEvent::ParticipantJoined(p.identity().0));
            }
            RoomEvent::ParticipantDisconnected(p) => {
                let id = p.identity().0;
                self.mixer.remove(&id);
                emit(CallEvent::ParticipantLeft(id));
            }
            RoomEvent::TrackSubscribed { track: RemoteTrack::Audio(audio), participant, .. } => {
                // A remote participant's audio: pull decoded frames at OUR
                // output device's rate/channels and feed this track's mixer
                // queue. libwebrtc resamples the decoder output for us.
                let id = participant.identity().0;
                let queue = self.mixer.add(id);
                let stream = NativeAudioStream::new(
                    audio.rtc_track(),
                    self.out_rate as i32,
                    self.out_channels as i32,
                );
                tokio::spawn(playback_pump(stream, queue));
            }
            RoomEvent::TrackUnsubscribed { participant, .. } => {
                self.mixer.remove(&participant.identity().0);
            }
            RoomEvent::ActiveSpeakersChanged { speakers } => {
                let names = speakers.iter().map(|p| p.identity().0).collect();
                emit(CallEvent::Speaking(names));
            }
            RoomEvent::Disconnected { reason } => {
                emit(CallEvent::Ended(format!("{reason:?}")));
            }
            _ => {}
        }
    }
}

/// Drain cpal mic samples, reassemble into exact 10ms frames, and hand them to
/// LiveKit. When muted, the frame is zeroed — we keep publishing silence so the
/// remote side still sees us as connected.
async fn capture_pump(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Vec<i16>>,
    source: NativeAudioSource,
    rate: u32,
    channels: u32,
    muted: Arc<AtomicBool>,
) {
    let frame_samples = (rate / 100) as usize; // per channel, 10ms
    let frame_len = frame_samples * channels as usize;
    let mut acc: VecDeque<i16> = VecDeque::with_capacity(frame_len * 4);

    while let Some(chunk) = rx.recv().await {
        acc.extend(chunk);
        while acc.len() >= frame_len {
            let mut data: Vec<i16> = acc.drain(..frame_len).collect();
            if muted.load(Ordering::Relaxed) {
                data.iter_mut().for_each(|s| *s = 0);
            }
            let frame = AudioFrame {
                data: data.into(),
                sample_rate: rate,
                num_channels: channels,
                samples_per_channel: frame_samples as u32,
            };
            // Errors here mean the track went away; end the pump quietly.
            if source.capture_frame(&frame).await.is_err() {
                return;
            }
        }
    }
}

/// Read decoded frames from one remote track and append to its mixer queue.
/// A soft cap keeps latency bounded if the consumer (speakers) falls behind.
async fn playback_pump(mut stream: NativeAudioStream, queue: TrackQueue) {
    const MAX_BUFFERED: usize = 48_000; // ~0.5s at 48k stereo; drop oldest past this
    while let Some(frame) = stream.next().await {
        let mut q = queue.lock().unwrap();
        q.extend(frame.data.iter().copied());
        while q.len() > MAX_BUFFERED {
            q.pop_front();
        }
    }
}

// ── cpal stream builders ────────────────────────────────────────────────────
// We support the two formats PipeWire/PulseAudio actually present — f32 and i16
// — and refuse anything else with a clear message rather than guessing.

fn build_input(
    device: &cpal::Device,
    cfg: &cpal::SupportedStreamConfig,
    tx: tokio::sync::mpsc::UnboundedSender<Vec<i16>>,
) -> Result<Stream, String> {
    let config = cfg.config();
    let err = |e| eprintln!("[voice] input stream error: {e}");
    let stream = match cfg.sample_format() {
        SampleFormat::F32 => device.build_input_stream(
            &config,
            move |data: &[f32], _| {
                let _ = tx.send(data.iter().map(|&s| f32_to_i16(s)).collect());
            },
            err,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream(
            &config,
            move |data: &[i16], _| {
                let _ = tx.send(data.to_vec());
            },
            err,
            None,
        ),
        other => return Err(format!("unsupported input format {other:?}")),
    };
    stream.map_err(|e| format!("build input stream: {e}"))
}

fn build_output(
    device: &cpal::Device,
    cfg: &cpal::SupportedStreamConfig,
    mixer: Mixer,
) -> Result<Stream, String> {
    let config = cfg.config();
    let err = |e| eprintln!("[voice] output stream error: {e}");
    let stream = match cfg.sample_format() {
        SampleFormat::F32 => device.build_output_stream(
            &config,
            move |data: &mut [f32], _| {
                for s in data.iter_mut() {
                    *s = i16_to_f32(mixer.next_sample());
                }
            },
            err,
            None,
        ),
        SampleFormat::I16 => device.build_output_stream(
            &config,
            move |data: &mut [i16], _| {
                for s in data.iter_mut() {
                    *s = mixer.next_sample();
                }
            },
            err,
            None,
        ),
        other => return Err(format!("unsupported output format {other:?}")),
    };
    stream.map_err(|e| format!("build output stream: {e}"))
}

#[inline]
fn f32_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

#[inline]
fn i16_to_f32(s: i16) -> f32 {
    s as f32 / (i16::MAX as f32)
}

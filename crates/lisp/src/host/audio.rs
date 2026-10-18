//! Audio output backend — the `audio-beep` builtin's engine (feature `audio`,
//! pulled in by `gui`). Like `gui.rs`, the symbol always exists; without the
//! feature it's a no-op, so the lean runtime links no audio stack.
//!
//! rodio's output stream is `!Send`, so (mirroring the gui thread) a dedicated
//! `brood-audio` thread owns the device and is fed `Beep` commands over a channel.
//! `beep` only sends — it never blocks the caller — so a game can fire a blip from
//! its frame loop with no latency. Muted (a graceful no-op) when there's no audio
//! device, when `BROOD_AUDIO=0`, or under `BROOD_GUI_HEADLESS` (so tests stay
//! silent). Beeps are synthesised sine tones mixed concurrently, so overlapping
//! sounds (a hit during a score jingle) just stack.

/// `(audio/beep freq-hz ms [vol])` — play a short tone at peak amplitude `vol`
/// (0..1). No-op without `--features audio`.
#[cfg(not(feature = "audio"))]
pub fn beep(_freq: f32, _ms: u64, _vol: f32) {}

#[cfg(feature = "audio")]
pub fn beep(freq: f32, ms: u64, vol: f32) {
    backend::beep(freq, ms, vol);
}

/// A decoded sound: interleaved f32 samples at `rate` Hz with `channels` channels —
/// what a `[:sound …]` op plays. Shared (`Arc`) so a window keeps one copy and each
/// play hands the mixer a reference, never the samples again.
#[derive(Debug)]
pub struct SoundData {
    pub rate: u32,
    pub channels: u16,
    pub samples: std::sync::Arc<[f32]>,
}

impl SoundData {
    /// From PCM16 little-endian bytes, the shape a Brood-side synthesiser or a WAV
    /// reader hands over; a stray trailing byte is dropped. Rate and channel counts of
    /// zero are normalised to something playable rather than refused.
    pub fn from_pcm16(rate: u32, channels: u16, pcm: &[u8]) -> SoundData {
        let samples: Vec<f32> = pcm
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c) as f32 / 32768.0)
            .collect();
        SoundData {
            rate: rate.clamp(1, 384_000),
            channels: channels.clamp(1, 2),
            samples: samples.into(),
        }
    }

    /// How long the sound plays, in seconds.
    pub fn seconds(&self) -> f32 {
        self.samples.len() as f32 / (self.rate as f32 * self.channels as f32)
    }
}

/// Play `sound` once at peak amplitude `vol` (0..1). Fire-and-forget like `beep`;
/// overlapping plays mix. No-op without `--features audio`.
#[cfg(not(feature = "audio"))]
pub fn play(_sound: &SoundData, _vol: f32) {}

#[cfg(feature = "audio")]
pub fn play(sound: &SoundData, vol: f32) {
    backend::play(sound, vol);
}

/// A playing track — what a `[:music …]` op keeps going. It loops until this handle is
/// dropped, which fades it out (a few tens of milliseconds, so a stop never clicks);
/// `set_volume` fades to the new level the same way. Without `--features audio` it is
/// an inert value, so a window can hold one either way.
pub struct Music {
    #[cfg(feature = "audio")]
    control: std::sync::Arc<backend::MusicControl>,
}

impl Music {
    /// Fade the track to peak amplitude `vol` (0..1; non-finite or <=0 is the default).
    pub fn set_volume(&self, _vol: f32) {
        #[cfg(feature = "audio")]
        self.control.set_volume(_vol);
    }
}

#[cfg(feature = "audio")]
impl Drop for Music {
    fn drop(&mut self) {
        self.control.stop();
    }
}

/// Start `sound` looping at peak amplitude `vol` (0..1). The track runs until the
/// returned `Music` is dropped. `None` when there is nothing to loop (an empty sound);
/// muted, or without `--features audio`, it is a handle to silence.
pub fn music(sound: &SoundData, vol: f32) -> Option<Music> {
    if sound.samples.is_empty() {
        return None;
    }
    #[cfg(feature = "audio")]
    return Some(Music {
        control: backend::music(sound, vol),
    });
    #[cfg(not(feature = "audio"))]
    {
        let _ = vol;
        Some(Music {})
    }
}

/// The longest a single `audio-beep` may run. `audio-beep` is fire-and-forget:
/// nothing ever stops a tone early, and the mixer holds each one until it ends.
/// So an unbounded duration is not "a long beep", it is a source that never
/// leaves the mixer — and since the primitive reaches this through
/// `ms.max(0.0) as u64`, a float `ms` of `1e300` saturates to `u64::MAX`
/// (~584 million years) instead of erroring. One stray expression in a frame loop
/// then piles up permanent tones for the process's life. 30 s is far past any
/// game blip or jingle and is what a *bounded* mistake costs.
#[cfg(feature = "audio")]
const MAX_BEEP_MS: u64 = 30_000;

/// The tone range a beep may ask for, in Hz — roughly human hearing. Outside it
/// there is nothing to hear, and the ends are actively bad: `SineWave::new` takes
/// the frequency straight into `sin(2*pi*f*t)`, so a NaN/inf `freq-hz` (which
/// `num_to_f64(…) as f32` will happily produce from `(/ 0.0 0.0)`) feeds NaN
/// samples into the shared mixer, where they contaminate every tone stacked
/// alongside them rather than just the bad one.
#[cfg(feature = "audio")]
const MIN_BEEP_HZ: f32 = 1.0;
#[cfg(feature = "audio")]
const MAX_BEEP_HZ: f32 = 20_000.0;

#[cfg(feature = "audio")]
mod backend {
    use rodio::source::SineWave;
    use rodio::{DeviceSinkBuilder, Source};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::mpsc::{self, Sender};
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;

    /// Default peak amplitude (0..1) — modest so stacked tones don't clip.
    const VOLUME: f32 = 0.18;

    enum Cmd {
        Beep { freq: f32, ms: u64, vol: f32 },
        Play { sound: super::SoundData, vol: f32 },
        Loop(LoopSource),
    }

    /// How long a track takes to fade to a new volume, or out when it stops.
    const FADE_SECONDS: f32 = 0.05;

    /// What a `Music` handle and its source on the mixer thread share: the volume the
    /// track is fading towards, and whether it has been stopped.
    pub struct MusicControl {
        vol_bits: AtomicU32,
        stopped: AtomicBool,
    }

    impl MusicControl {
        pub fn set_volume(&self, vol: f32) {
            self.vol_bits
                .store(clamp_volume(vol).to_bits(), Ordering::Relaxed);
        }

        pub fn stop(&self) {
            self.stopped.store(true, Ordering::Relaxed);
        }

        fn target(&self) -> f32 {
            if self.stopped.load(Ordering::Relaxed) {
                0.0
            } else {
                f32::from_bits(self.vol_bits.load(Ordering::Relaxed))
            }
        }
    }

    /// A sound played round and round, its gain ramped towards the control's target a
    /// sample at a time. It ends once stopped and faded to silence.
    pub struct LoopSource {
        samples: std::sync::Arc<[f32]>,
        position: usize,
        channels: std::num::NonZeroU16,
        rate: std::num::NonZeroU32,
        gain: f32,
        step: f32,
        control: Arc<MusicControl>,
    }

    impl Iterator for LoopSource {
        type Item = f32;

        fn next(&mut self) -> Option<f32> {
            let target = self.control.target();
            // Only end on a frame boundary, so the next source's channels stay aligned.
            if target == 0.0
                && self.gain == 0.0
                && self.position.is_multiple_of(self.channels.get() as usize)
                && self.control.stopped.load(Ordering::Relaxed)
            {
                return None;
            }
            if self.gain < target {
                self.gain = (self.gain + self.step).min(target);
            } else if self.gain > target {
                self.gain = (self.gain - self.step).max(target);
            }
            let sample = self.samples[self.position] * self.gain;
            self.position = (self.position + 1) % self.samples.len();
            Some(sample)
        }
    }

    impl Source for LoopSource {
        fn current_span_len(&self) -> Option<usize> {
            None
        }

        fn channels(&self) -> std::num::NonZeroU16 {
            self.channels
        }

        fn sample_rate(&self) -> std::num::NonZeroU32 {
            self.rate
        }

        fn total_duration(&self) -> Option<Duration> {
            None
        }
    }

    fn clamp_volume(vol: f32) -> f32 {
        if vol.is_finite() && vol > 0.0 {
            vol.min(1.0)
        } else {
            VOLUME
        }
    }

    /// Start `sound` looping, faded in to `vol`; the control is what its `Music` holds.
    /// Muted, the source is never sent and the control steers nothing.
    pub fn music(sound: &super::SoundData, vol: f32) -> Arc<MusicControl> {
        let (control, source) = looping(sound, vol);
        if let (Some(source), Some(tx)) = (source, sender()) {
            let _ = tx.send(Cmd::Loop(source));
        }
        control
    }

    /// The control and the source for `sound` looping at `vol` — no source for a sound
    /// with a zero rate or channel count, which has nothing to play.
    fn looping(sound: &super::SoundData, vol: f32) -> (Arc<MusicControl>, Option<LoopSource>) {
        let control = Arc::new(MusicControl {
            vol_bits: AtomicU32::new(clamp_volume(vol).to_bits()),
            stopped: AtomicBool::new(false),
        });
        let (Some(channels), Some(rate)) = (
            std::num::NonZeroU16::new(sound.channels),
            std::num::NonZeroU32::new(sound.rate),
        ) else {
            return (control, None);
        };
        let per_second = sound.rate as f32 * sound.channels as f32;
        let source = LoopSource {
            samples: sound.samples.clone(),
            position: 0,
            channels,
            rate,
            gain: 0.0,
            step: 1.0 / (per_second * FADE_SECONDS).max(1.0),
            control: control.clone(),
        };
        (control, Some(source))
    }

    fn muted() -> bool {
        let on = |k: &str| {
            std::env::var(k)
                .map(|v| v != "0" && !v.is_empty())
                .unwrap_or(false)
        };
        on("BROOD_GUI_HEADLESS")
            || std::env::var("BROOD_AUDIO")
                .map(|v| v == "0")
                .unwrap_or(false)
    }

    /// The channel to the audio thread, started on first use. `None` when muted or
    /// the thread couldn't start; the audio thread itself exits quietly if there's
    /// no output device, after which sends are harmless no-ops.
    ///
    /// Only a STARTED thread is kept: the OS can refuse one (`EAGAIN` under a thread
    /// limit), and caching that refusal would mute the process for good. The next sound
    /// tries again, as the timer and the network reactor do. `STARTING` keeps two first
    /// callers from starting two threads.
    fn sender() -> Option<&'static Sender<Cmd>> {
        static SENDER: OnceLock<Sender<Cmd>> = OnceLock::new();
        static STARTING: std::sync::Mutex<()> = std::sync::Mutex::new(());
        if let Some(sender) = SENDER.get() {
            return Some(sender);
        }
        if muted() {
            return None;
        }
        let _starting = crate::core::sync::lock(&STARTING);
        if let Some(sender) = SENDER.get() {
            return Some(sender);
        }
        let (tx, rx) = mpsc::channel::<Cmd>();
        let started = std::thread::Builder::new()
            .name("brood-audio".into())
            .spawn(move || {
                // Own the device for the thread's life (the sink must stay alive).
                let stream = match DeviceSinkBuilder::open_default_sink() {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let mixer = stream.mixer();
                while let Ok(cmd) = rx.recv() {
                    // `add` mixes concurrently, so overlapping sounds stack.
                    match cmd {
                        Cmd::Beep { freq, ms, vol } => {
                            let tone = SineWave::new(freq)
                                .take_duration(Duration::from_millis(ms))
                                .amplify(vol);
                            mixer.add(tone);
                        }
                        Cmd::Play { sound, vol } => {
                            let (Some(channels), Some(rate)) = (
                                std::num::NonZeroU16::new(sound.channels),
                                std::num::NonZeroU32::new(sound.rate),
                            ) else {
                                continue;
                            };
                            let buffer = rodio::buffer::SamplesBuffer::new(
                                channels,
                                rate,
                                sound.samples.to_vec(),
                            )
                            .amplify(vol);
                            mixer.add(buffer);
                        }
                        Cmd::Loop(source) => mixer.add(source),
                    }
                }
            });
        match started {
            Ok(_) => Some(SENDER.get_or_init(|| tx)),
            Err(_) => None,
        }
    }

    pub fn beep(freq: f32, ms: u64, vol: f32) {
        use super::{MAX_BEEP_HZ, MAX_BEEP_MS, MIN_BEEP_HZ};
        // Clamp to a sane amplitude; non-finite or <=0 falls back to the default.
        let vol = if vol.is_finite() && vol > 0.0 {
            vol.min(1.0)
        } else {
            VOLUME
        };
        // A non-finite frequency has no tone to play and would poison the shared
        // mixer with NaN samples, so it is dropped rather than clamped — unlike
        // `vol`, there is no sensible default pitch to substitute. A finite one is
        // clamped into the audible band. A zero-length beep is likewise nothing to
        // play, and the length is capped so a fire-and-forget tone always ends.
        if !freq.is_finite() || ms == 0 {
            return;
        }
        let freq = freq.clamp(MIN_BEEP_HZ, MAX_BEEP_HZ);
        let ms = ms.min(MAX_BEEP_MS);
        if let Some(tx) = sender() {
            let _ = tx.send(Cmd::Beep { freq, ms, vol });
        }
    }

    /// A decoded sound to the mixer, once. The samples are shared, so this is a
    /// reference count, not a copy, per play; a non-finite or non-positive `vol` takes
    /// the default; an empty sound is nothing to play.
    pub fn play(sound: &super::SoundData, vol: f32) {
        let vol = if vol.is_finite() && vol > 0.0 {
            vol.min(1.0)
        } else {
            VOLUME
        };
        if sound.samples.is_empty() {
            return;
        }
        if let Some(tx) = sender() {
            let _ = tx.send(Cmd::Play {
                sound: super::SoundData {
                    rate: sound.rate,
                    channels: sound.channels,
                    samples: sound.samples.clone(),
                },
                vol,
            });
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn constant(rate: u32, channels: u16, len: usize) -> super::super::SoundData {
            super::super::SoundData {
                rate,
                channels,
                samples: vec![1.0; len].into(),
            }
        }

        #[test]
        fn a_track_fades_in_loops_and_ends_after_it_is_stopped() {
            // 1000 Hz mono: the 50 ms fade is 50 samples.
            let (control, source) = looping(&constant(1000, 1, 7), 0.5);
            let mut source = source.unwrap();
            let start: Vec<f32> = source.by_ref().take(100).collect();
            assert!(start[0] > 0.0 && start[0] < 0.05, "fades in: {}", start[0]);
            assert_eq!(start[99], 0.5, "reaches the volume, past the 7-sample end");
            control.set_volume(0.25);
            assert_eq!(source.by_ref().nth(60), Some(0.25), "fades to a new volume");
            control.stop();
            let tail: Vec<f32> = source.by_ref().take(1000).collect();
            assert!(
                tail.len() < 40,
                "ends once faded out: {} samples",
                tail.len()
            );
            assert!(tail.windows(2).all(|w| w[1] <= w[0]), "a fade, not a cut");
        }

        #[test]
        fn a_stereo_track_ends_on_a_frame_boundary() {
            let (control, source) = looping(&constant(1000, 2, 4), 1.0);
            let mut source = source.unwrap();
            source.by_ref().take(201).count();
            control.stop();
            let played = 201 + source.count();
            assert_eq!(
                played % 2,
                0,
                "{played} samples is a whole number of frames"
            );
        }
    }
}

//! Ambient sound mixer: procedural white/pink/brown noise plus user files from
//! `<data_dir>/sounds/`. Runs on a dedicated thread because the audio output
//! handle is not `Send`.

use rodio::{ChannelCount, DeviceSinkBuilder, Player, SampleRate, Source};
use std::collections::HashMap;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

pub const SAMPLE_RATE: u32 = 44_100;
pub const EXTENSIONS: &[&str] = &["ogg", "mp3", "wav", "flac"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseKind {
    White,
    Pink,
    Brown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackSpec {
    Noise(NoiseKind),
    File(PathBuf),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackInfo {
    pub id: String,
    pub name: String,
    pub spec: TrackSpec,
}

pub fn builtin_tracks() -> Vec<TrackInfo> {
    vec![
        TrackInfo { id: "white".into(), name: "White noise".into(), spec: TrackSpec::Noise(NoiseKind::White) },
        TrackInfo { id: "pink".into(), name: "Pink noise".into(), spec: TrackSpec::Noise(NoiseKind::Pink) },
        TrackInfo { id: "brown".into(), name: "Brown noise".into(), spec: TrackSpec::Noise(NoiseKind::Brown) },
    ]
}

/// Built-in tracks followed by every supported audio file in `dir`.
pub fn scan_tracks(dir: &Path) -> Vec<TrackInfo> {
    let mut v = builtin_tracks();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.extension()
                            .and_then(|e| e.to_str())
                            .is_some_and(|e| EXTENSIONS.contains(&e.to_lowercase().as_str()))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    for p in files {
        let fname = p.file_name().and_then(|f| f.to_str()).unwrap_or("?").to_string();
        let stem = p.file_stem().and_then(|f| f.to_str()).unwrap_or("?").to_string();
        v.push(TrackInfo { id: format!("file:{fname}"), name: stem, spec: TrackSpec::File(p) });
    }
    v
}

// ---------------------------------------------------------------------------
// Noise generation
// ---------------------------------------------------------------------------

/// Small, fast xorshift PRNG: noise quality needs are modest and this avoids
/// any dependency on `rand` API details in the audio path.
#[derive(Clone)]
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        XorShift(seed | 1)
    }
    fn next_f32(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        // map to [-1, 1)
        ((x >> 40) as f32 / (1u64 << 23) as f32) - 1.0
    }
}

#[derive(Clone, Default)]
struct PinkState {
    b0: f32,
    b1: f32,
    b2: f32,
    b3: f32,
    b4: f32,
    b5: f32,
    b6: f32,
}

/// Generator for one channel of noise.
#[derive(Clone)]
pub struct NoiseChannel {
    kind: NoiseKind,
    rng: XorShift,
    pink: PinkState,
    brown: f32,
}

impl NoiseChannel {
    pub fn new(kind: NoiseKind, seed: u64) -> Self {
        Self { kind, rng: XorShift::new(seed), pink: PinkState::default(), brown: 0.0 }
    }

    // Paul Kellet's published filter coefficients, kept verbatim.
    #[allow(clippy::excessive_precision)]
    pub fn next_sample(&mut self) -> f32 {
        let white = self.rng.next_f32();
        match self.kind {
            NoiseKind::White => white * 0.5,
            NoiseKind::Pink => {
                // Paul Kellet's refined pink filter.
                let p = &mut self.pink;
                p.b0 = 0.99886 * p.b0 + white * 0.0555179;
                p.b1 = 0.99332 * p.b1 + white * 0.0750759;
                p.b2 = 0.96900 * p.b2 + white * 0.1538520;
                p.b3 = 0.86650 * p.b3 + white * 0.3104856;
                p.b4 = 0.55000 * p.b4 + white * 0.5329522;
                p.b5 = -0.7616 * p.b5 - white * 0.0168980;
                let out = p.b0 + p.b1 + p.b2 + p.b3 + p.b4 + p.b5 + p.b6 + white * 0.5362;
                p.b6 = white * 0.115926;
                (out * 0.11).clamp(-1.0, 1.0)
            }
            NoiseKind::Brown => {
                // Leaky integrator of white noise.
                self.brown = (self.brown + 0.02 * white) / 1.02;
                (self.brown * 3.5).clamp(-1.0, 1.0)
            }
        }
    }
}

/// Infinite stereo noise source (independent channels).
pub struct NoiseSource {
    left: NoiseChannel,
    right: NoiseChannel,
    next_is_left: bool,
}

impl NoiseSource {
    pub fn new(kind: NoiseKind) -> Self {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9);
        Self {
            left: NoiseChannel::new(kind, t ^ 0xA5A5_A5A5_1234_5678),
            right: NoiseChannel::new(kind, t.rotate_left(17) ^ 0x0F0F_0F0F_8765_4321),
            next_is_left: true,
        }
    }
}

impl Iterator for NoiseSource {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        let s = if self.next_is_left { self.left.next_sample() } else { self.right.next_sample() };
        self.next_is_left = !self.next_is_left;
        Some(s)
    }
}

impl Source for NoiseSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> ChannelCount {
        NonZero::new(2).unwrap()
    }
    fn sample_rate(&self) -> SampleRate {
        NonZero::new(SAMPLE_RATE).unwrap()
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// Short two-tone chime as raw stereo samples.
pub fn chime_samples() -> Vec<f32> {
    let rate = SAMPLE_RATE as f32;
    let mut out = Vec::new();
    for (freq, dur) in [(880.0f32, 0.18f32), (1318.5, 0.32)] {
        let n = (rate * dur) as usize;
        for i in 0..n {
            let t = i as f32 / rate;
            let env = (1.0 - i as f32 / n as f32).powf(1.5) * (i as f32 / 200.0).min(1.0);
            let s = (2.0 * std::f32::consts::PI * freq * t).sin() * 0.25 * env;
            out.push(s);
            out.push(s);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Engine thread
// ---------------------------------------------------------------------------

pub enum AudioCmd {
    Master(f32),
    Track { id: String, spec: TrackSpec, volume: f32 },
    SetPlaying(bool),
    Chime(f32),
    Quit,
}

#[derive(Clone)]
pub struct Audio {
    tx: Sender<AudioCmd>,
}

impl Audio {
    /// Start the audio thread. `status` receives human-readable status text
    /// ("Audio ready" / error) so the UI can show a message instead of failing.
    pub fn spawn(status: impl Fn(String) + Send + 'static) -> Audio {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new().name("audio".into()).spawn(move || engine(rx, status)).expect("spawn audio thread");
        Audio { tx }
    }
    pub fn send(&self, c: AudioCmd) {
        let _ = self.tx.send(c);
    }
}

struct Slot {
    volume: f32,
    spec: TrackSpec,
    player: Option<Player>,
}

fn make_source(spec: &TrackSpec) -> Option<Box<dyn Source + Send>> {
    match spec {
        TrackSpec::Noise(k) => Some(Box::new(NoiseSource::new(*k))),
        TrackSpec::File(p) => {
            let f = std::fs::File::open(p).ok()?;
            // Looped decoder seeks back to the start: gapless loop.
            let d = rodio::Decoder::new_looped(std::io::BufReader::new(f))
                .map_err(|e| tracing::warn!("decode {}: {e}", p.display()))
                .ok()?;
            Some(Box::new(d))
        }
    }
}

fn engine(rx: Receiver<AudioCmd>, status: impl Fn(String)) {
    let sink = match DeviceSinkBuilder::open_default_sink() {
        Ok(s) => {
            status("Audio ready".into());
            Some(s)
        }
        Err(e) => {
            tracing::warn!("no audio output: {e}");
            status(format!("No audio output available ({e})"));
            None
        }
    };
    let mut slots: HashMap<String, Slot> = HashMap::new();
    let mut master = 0.8f32;
    let mut playing = false;

    let sync = |slots: &mut HashMap<String, Slot>, master: f32, playing: bool| {
        let Some(sink) = &sink else { return };
        // Headroom: the mixer sums tracks, so scale by 1/sqrt(n) to avoid clipping
        // when several tracks are turned up at once.
        let active = slots.values().filter(|s| playing && s.volume > 0.001).count().max(1);
        let headroom = 1.0 / (active as f32).sqrt();
        for (id, slot) in slots.iter_mut() {
            let want = playing && slot.volume > 0.001;
            match (&slot.player, want) {
                (None, true) => {
                    if let Some(src) = make_source(&slot.spec) {
                        let p = Player::connect_new(sink.mixer());
                        p.set_volume(slot.volume * master * headroom);
                        p.append(src);
                        slot.player = Some(p);
                    } else {
                        tracing::warn!("cannot play track {id}");
                    }
                }
                (Some(_), false) => {
                    if let Some(p) = slot.player.take() {
                        p.stop();
                    }
                }
                (Some(p), true) => p.set_volume(slot.volume * master * headroom),
                (None, false) => {}
            }
        }
    };

    while let Ok(cmd) = rx.recv() {
        match cmd {
            AudioCmd::Quit => break,
            AudioCmd::Master(v) => {
                master = v.clamp(0.0, 1.0);
                sync(&mut slots, master, playing);
            }
            AudioCmd::Track { id, spec, volume } => {
                let slot = slots.entry(id).or_insert(Slot { volume: 0.0, spec: spec.clone(), player: None });
                slot.volume = volume.clamp(0.0, 1.0);
                slot.spec = spec;
                sync(&mut slots, master, playing);
            }
            AudioCmd::SetPlaying(p) => {
                playing = p;
                sync(&mut slots, master, playing);
            }
            AudioCmd::Chime(vol) => {
                if let Some(sink) = &sink {
                    let p = Player::connect_new(sink.mixer());
                    p.set_volume(vol.clamp(0.0, 1.0));
                    p.append(rodio::buffer::SamplesBuffer::new(
                        NonZero::new(2).unwrap(),
                        NonZero::new(SAMPLE_RATE).unwrap(),
                        chime_samples(),
                    ));
                    p.detach();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen(kind: NoiseKind, n: usize) -> Vec<f32> {
        let mut c = NoiseChannel::new(kind, 42);
        (0..n).map(|_| c.next_sample()).collect()
    }

    fn lag1(v: &[f32]) -> f32 {
        let mean = v.iter().sum::<f32>() / v.len() as f32;
        let var: f32 = v.iter().map(|x| (x - mean).powi(2)).sum();
        let cov: f32 = v.windows(2).map(|w| (w[0] - mean) * (w[1] - mean)).sum();
        cov / var
    }

    #[test]
    fn all_noise_is_bounded_and_centered() {
        for k in [NoiseKind::White, NoiseKind::Pink, NoiseKind::Brown] {
            let v = gen(k, 200_000);
            assert!(v.iter().all(|x| x.abs() <= 1.0 && x.is_finite()), "{k:?} out of range");
            let mean = v.iter().sum::<f32>() / v.len() as f32;
            assert!(mean.abs() < 0.05, "{k:?} mean {mean}");
            let rms = (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();
            assert!(rms > 0.02, "{k:?} too quiet: {rms}");
        }
    }

    #[test]
    fn spectral_colour_ordering() {
        // Lag-1 autocorrelation grows white < pink < brown (more low-frequency energy).
        let w = lag1(&gen(NoiseKind::White, 100_000));
        let p = lag1(&gen(NoiseKind::Pink, 100_000));
        let b = lag1(&gen(NoiseKind::Brown, 100_000));
        assert!(w.abs() < 0.05, "white {w}");
        assert!(p > 0.3 && p < b, "pink {p} brown {b}");
        assert!(b > 0.9, "brown {b}");
    }

    #[test]
    fn source_is_stereo_infinite() {
        let mut s = NoiseSource::new(NoiseKind::Pink);
        assert_eq!(s.channels().get(), 2);
        assert!(s.total_duration().is_none());
        assert!(s.by_ref().take(10_000).all(|x| x.abs() <= 1.0));
    }

    #[test]
    fn scans_sound_dir() {
        let d = tempfile::tempdir().unwrap();
        for n in ["rain.ogg", "cafe.MP3", "notes.txt", "lofi.flac", "ding.wav"] {
            std::fs::write(d.path().join(n), b"x").unwrap();
        }
        std::fs::create_dir(d.path().join("sub.ogg")).unwrap();
        let t = scan_tracks(d.path());
        let ids: Vec<_> = t.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(&ids[..3], &["white", "pink", "brown"]);
        assert_eq!(&ids[3..], &["file:cafe.MP3", "file:ding.wav", "file:lofi.flac", "file:rain.ogg"]);
        assert_eq!(scan_tracks(Path::new("/nonexistent-dir-xyz")).len(), 3);
    }

    #[test]
    fn chime_is_sane() {
        let c = chime_samples();
        assert!(c.len() > 10_000 && c.iter().all(|x| x.abs() <= 0.3));
    }
}

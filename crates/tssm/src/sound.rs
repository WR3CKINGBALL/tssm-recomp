//! Host sound output. The mixer pushes 32 kHz stereo frames into a queue; the audio device's
//! callback drains it, resampling to whatever rate the device runs at.

use std::collections::VecDeque;
use std::sync::Mutex;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// The console's audio rate.
pub const SOURCE_RATE: u32 = 32_000;
/// Cap on queued audio (200 ms). Beyond it the oldest frames are dropped, so latency cannot
/// build up if the game runs ahead of the device.
const MAX_QUEUED: usize = SOURCE_RATE as usize / 5;

static QUEUE: Mutex<VecDeque<[i16; 2]>> = Mutex::new(VecDeque::new());

/// Appends mixed frames to the WAV file named by TSSM_DUMP_AUDIO, for checking the mixer's
/// output without listening. The header claims the maximum length because the game is usually
/// killed rather than exited; players and tools read up to the real end of file.
fn dump(frames: &[[i16; 2]]) {
    use std::io::Write;
    static FILE: Mutex<Option<Option<std::fs::File>>> = Mutex::new(None);
    let mut slot = FILE.lock().unwrap();
    let file = slot.get_or_insert_with(|| {
        let path = std::env::var_os("TSSM_DUMP_AUDIO")?;
        let mut file = std::fs::File::create(path).ok()?;
        let mut header = Vec::new();
        header.extend_from_slice(b"RIFF\xFF\xFF\xFF\x7FWAVEfmt ");
        header.extend_from_slice(&16u32.to_le_bytes());
        header.extend_from_slice(&1u16.to_le_bytes()); // PCM
        header.extend_from_slice(&2u16.to_le_bytes()); // stereo
        header.extend_from_slice(&SOURCE_RATE.to_le_bytes());
        header.extend_from_slice(&(SOURCE_RATE * 4).to_le_bytes());
        header.extend_from_slice(&4u16.to_le_bytes());
        header.extend_from_slice(&16u16.to_le_bytes());
        header.extend_from_slice(b"data\xDB\xFF\xFF\x7F");
        file.write_all(&header).ok()?;
        Some(file)
    });
    if let Some(file) = file {
        let bytes: Vec<u8> = frames.iter().flat_map(|f| [f[0].to_le_bytes(), f[1].to_le_bytes()]).flatten().collect();
        let _ = file.write_all(&bytes);
    }
}

/// Queues mixed stereo frames (left, right) for playback.
pub fn push(frames: &[[i16; 2]]) {
    dump(frames);
    let mut queue = QUEUE.lock().unwrap();
    queue.extend(frames.iter().copied());
    let excess = queue.len().saturating_sub(MAX_QUEUED);
    queue.drain(..excess);
}

/// Opens the default output device on a thread of its own, which then keeps the stream alive.
pub fn start() {
    std::thread::spawn(|| {
        let Some(device) = cpal::default_host().default_output_device() else {
            eprintln!("[sound] no output device; running silent");
            return;
        };
        let config = match device.default_output_config() {
            Ok(config) => config,
            Err(e) => return eprintln!("[sound] cannot query output device: {e}"),
        };
        if config.sample_format() != cpal::SampleFormat::F32 {
            return eprintln!("[sound] unsupported device sample format {:?}; running silent", config.sample_format());
        }
        let channels = config.channels() as usize;
        let device_rate = u32::from(config.sample_rate());
        // Source frames consumed per device frame.
        let step = SOURCE_RATE as f64 / device_rate as f64;
        let (mut position, mut current, mut next) = (0.0f64, [0.0f32; 2], [0.0f32; 2]);

        let stream = device.build_output_stream(
            config.config(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut queue = QUEUE.lock().unwrap();
                for frame in data.chunks_mut(channels) {
                    position += step;
                    while position >= 1.0 {
                        position -= 1.0;
                        current = next;
                        // An empty queue plays as silence.
                        next = queue.pop_front().map_or([0.0; 2], |s| s.map(|v| v as f32 / 32768.0));
                    }
                    for (channel, out) in frame.iter_mut().enumerate() {
                        *out = match channel {
                            0 | 1 => current[channel] + (next[channel] - current[channel]) * position as f32,
                            _ => 0.0,
                        };
                    }
                }
            },
            |e| eprintln!("[sound] stream error: {e}"),
            None,
        );
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => return eprintln!("[sound] cannot open output stream: {e}"),
        };
        if let Err(e) = stream.play() {
            return eprintln!("[sound] cannot start output stream: {e}");
        }
        eprintln!("[sound] playing at {device_rate} Hz, {channels} channels");
        loop {
            std::thread::park(); // the stream stops if dropped
        }
    });
}

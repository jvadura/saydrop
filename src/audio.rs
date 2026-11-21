//! Microphone capture through the classic waveIn API, plus the WAV encoding
//! the speech server receives.
//!
//! The device is opened at 16 kHz mono 16-bit, which is what speech models
//! work at; Windows converts from whatever the microphone delivers. Buffers
//! come back as window messages on the UI thread, so there is no audio
//! thread and no locking.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::path::Path;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Media::Audio::{
    CALLBACK_WINDOW, WAVE_FORMAT_PCM, WAVE_MAPPER, WAVEFORMATEX, WAVEHDR, WAVEINCAPSW, WHDR_DONE,
    waveInAddBuffer, waveInClose, waveInGetDevCapsW, waveInGetNumDevs, waveInOpen,
    waveInPrepareHeader, waveInReset, waveInStart, waveInUnprepareHeader,
};

use crate::util::from_wide;

pub const SAMPLE_RATE: u32 = 16_000;
/// 50 ms per buffer: often enough for a lively level meter.
const BUFFER_SAMPLES: usize = 800;
const BUFFER_COUNT: usize = 8;
/// Hard stop for runaway recordings when no limit is configured: one hour.
const MAX_SAMPLES: usize = SAMPLE_RATE as usize * 3600;
/// Lets the wave mapper convert formats for a specific device as well.
const WAVE_MAPPED: u32 = 0x0004;
const HEADER_SIZE: u32 = size_of::<WAVEHDR>() as u32;

struct Buffer {
    header: WAVEHDR,
    data: Vec<i16>,
}

pub struct Recorder {
    handle: *mut c_void,
    /// Boxed so the headers keep their address while the driver holds them.
    #[allow(clippy::vec_box)]
    buffers: Vec<Box<Buffer>>,
    /// Indexes of the buffers currently with the driver, oldest first.
    queued: VecDeque<usize>,
    samples: Vec<i16>,
}

/// Names of the recording devices, in device-id order.
pub fn devices() -> Vec<String> {
    // SAFETY: `caps` is a correctly sized out-structure.
    unsafe {
        (0..waveInGetNumDevs())
            .filter_map(|id| {
                let mut caps: WAVEINCAPSW = std::mem::zeroed();
                let status =
                    waveInGetDevCapsW(id as usize, &mut caps, size_of::<WAVEINCAPSW>() as u32);
                (status == 0).then(|| from_wide(&{ caps.szPname }))
            })
            .collect()
    }
}

fn open_error(code: u32) -> String {
    match code {
        2 | 6 => "The microphone is not available. Check Windows sound settings.".to_string(),
        4 => "The microphone is in use by another program.".to_string(),
        32 => "The microphone does not support 16 kHz recording.".to_string(),
        other => format!("The microphone could not be opened (error {other})."),
    }
}

impl Recorder {
    /// Start recording. `device` is a name from `devices()`, or empty for
    /// the Windows default. Buffers arrive at `hwnd` as `MM_WIM_DATA`.
    /// The second value is a warning when the named device was not found.
    pub fn start(hwnd: HWND, device: &str) -> Result<(Recorder, Option<String>), String> {
        let names = devices();
        if names.is_empty() {
            return Err(
                "No microphone found. Plug one in, or over Remote Desktop enable audio recording."
                    .to_string(),
            );
        }
        let mut warning = None;
        let (device_id, flags) = match names.iter().position(|n| n == device) {
            Some(index) => (index as u32, CALLBACK_WINDOW | WAVE_MAPPED),
            None => {
                if !device.is_empty() {
                    warning = Some(format!(
                        "Microphone \"{device}\" was not found; using the Windows default."
                    ));
                }
                (WAVE_MAPPER, CALLBACK_WINDOW)
            }
        };

        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 1,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * 2,
            nBlockAlign: 2,
            wBitsPerSample: 16,
            cbSize: 0,
        };

        // SAFETY: the format and handle pointers are valid for the call. Each
        // header points at sample storage owned by the same boxed `Buffer`,
        // which is kept alive until the device is reset and closed in `Drop`.
        unsafe {
            let mut handle = std::ptr::null_mut();
            let status = waveInOpen(&mut handle, device_id, &format, hwnd as usize, 0, flags);
            if status != 0 {
                return Err(open_error(status));
            }
            let mut recorder = Recorder {
                handle,
                buffers: Vec::with_capacity(BUFFER_COUNT),
                queued: VecDeque::with_capacity(BUFFER_COUNT),
                samples: Vec::with_capacity(SAMPLE_RATE as usize * 30),
            };
            for index in 0..BUFFER_COUNT {
                let mut buffer = Box::new(Buffer {
                    header: std::mem::zeroed(),
                    data: vec![0i16; BUFFER_SAMPLES],
                });
                buffer.header.lpData = buffer.data.as_mut_ptr().cast();
                buffer.header.dwBufferLength = (BUFFER_SAMPLES * 2) as u32;
                let header = &raw mut buffer.header;
                recorder.buffers.push(buffer);
                if waveInPrepareHeader(handle, header, HEADER_SIZE) != 0
                    || waveInAddBuffer(handle, header, HEADER_SIZE) != 0
                {
                    return Err("The microphone refused the recording buffers.".to_string());
                }
                recorder.queued.push_back(index);
            }
            let status = waveInStart(handle);
            if status != 0 {
                return Err(open_error(status));
            }
            Ok((recorder, warning))
        }
    }

    /// Handle one `MM_WIM_DATA` message (`lparam` is the header pointer).
    /// Returns the peak level of the new audio, 0.0 to 1.0, or `None` when
    /// the message belongs to an earlier recording.
    pub fn on_data(&mut self, header: *const WAVEHDR) -> Option<f32> {
        let index = self
            .buffers
            .iter()
            .position(|b| std::ptr::eq(&raw const b.header, header))?;
        let place = self.queued.iter().position(|&i| i == index)?;
        // A leftover message from an earlier recording could name a header
        // at a reused address; only a buffer the driver has finished counts.
        if { self.buffers[index].header.dwFlags } & WHDR_DONE == 0 {
            return None;
        }
        self.queued.remove(place);
        let peak = self.collect(index);

        if self.samples.len() < MAX_SAMPLES {
            let header = &raw mut self.buffers[index].header;
            // SAFETY: the header was prepared in `start` and is not queued.
            if unsafe { waveInAddBuffer(self.handle, header, HEADER_SIZE) } == 0 {
                self.queued.push_back(index);
            }
        }
        Some(peak)
    }

    /// Append what the driver wrote into buffer `index`; returns its peak.
    fn collect(&mut self, index: usize) -> f32 {
        let buffer = &self.buffers[index];
        let recorded = { buffer.header.dwBytesRecorded } as usize / 2;
        let chunk = &buffer.data[..recorded.min(buffer.data.len())];
        let peak = chunk.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        self.samples.extend_from_slice(chunk);
        peak as f32 / 32768.0
    }

    /// Stop and return everything recorded, including the audio still
    /// sitting in the driver's buffers.
    pub fn finish(mut self) -> Vec<i16> {
        // SAFETY: our own open device. Reset hands every queued buffer back,
        // marked done, with the amount recorded so far.
        unsafe { waveInReset(self.handle) };
        while let Some(index) = self.queued.pop_front() {
            if { self.buffers[index].header.dwFlags } & WHDR_DONE != 0 {
                self.collect(index);
            }
        }
        std::mem::take(&mut self.samples)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // SAFETY: reset returns all buffers to us, after which they can be
        // unprepared and the device closed. The buffers are freed after this.
        unsafe {
            waveInReset(self.handle);
            for buffer in &mut self.buffers {
                waveInUnprepareHeader(self.handle, &raw mut buffer.header, HEADER_SIZE);
            }
            waveInClose(self.handle);
        }
    }
}

/// Loudest sample of a recording, 0.0 to 1.0.
pub fn peak(samples: &[i16]) -> f32 {
    samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0) as f32 / 32768.0
}

/// A complete 16-bit mono WAV file for `samples`.
pub fn wav_bytes(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    let data_bytes = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend(b"RIFF");
    out.extend((36 + data_bytes).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes()); // PCM
    out.extend(1u16.to_le_bytes()); // mono
    out.extend(sample_rate.to_le_bytes());
    out.extend((sample_rate * 2).to_le_bytes());
    out.extend(2u16.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend(data_bytes.to_le_bytes());
    for sample in samples {
        out.extend(sample.to_le_bytes());
    }
    out
}

/// Read a 16-bit PCM WAV file as 16 kHz mono samples. Used by `--mic-file`,
/// which stands in for the microphone when testing.
pub fn read_wav(path: &Path) -> Result<Vec<i16>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    decode_wav(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

fn decode_wav(bytes: &[u8]) -> Result<Vec<i16>, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV file".to_string());
    }
    let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let u32_at =
        |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);

    let mut format: Option<(u16, u16, u32, u16)> = None;
    let mut data: Option<&[u8]> = None;
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32_at(at + 4) as usize;
        let body = at + 8;
        let end = body.saturating_add(size).min(bytes.len());
        if id == b"fmt " && end - body >= 16 {
            format = Some((
                u16_at(body),
                u16_at(body + 2),
                u32_at(body + 4),
                u16_at(body + 14),
            ));
        } else if id == b"data" {
            data = Some(&bytes[body..end]);
        }
        at = end + (size & 1);
    }

    let (tag, channels, rate, bits) = format.ok_or("no format chunk")?;
    let data = data.ok_or("no audio data")?;
    // 0xFFFE is WAVE_FORMAT_EXTENSIBLE, which wraps plain PCM here.
    if !(tag == 1 || tag == 0xFFFE) || bits != 16 || channels == 0 || rate == 0 {
        return Err("only 16-bit PCM WAV files are supported".to_string());
    }

    let channels = channels as usize;
    let mono: Vec<i16> = data
        .chunks_exact(2 * channels)
        .map(|frame| {
            let sum: i32 = frame
                .as_chunks::<2>()
                .0
                .iter()
                .map(|s| i16::from_le_bytes(*s) as i32)
                .sum();
            (sum / channels as i32) as i16
        })
        .collect();
    Ok(resample(&mono, rate, SAMPLE_RATE))
}

/// Linear resampling: plain, and good enough for a test input.
fn resample(samples: &[i16], from: u32, to: u32) -> Vec<i16> {
    if from == to || samples.is_empty() {
        return samples.to_vec();
    }
    let length = (samples.len() as u64 * to as u64 / from as u64) as usize;
    (0..length)
        .map(|i| {
            let position = i as f64 * from as f64 / to as f64;
            let index = position as usize;
            let next = (index + 1).min(samples.len() - 1);
            let fraction = position - index as f64;
            (samples[index] as f64 * (1.0 - fraction) + samples[next] as f64 * fraction) as i16
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_round_trip() {
        let samples: Vec<i16> = (0..1600).map(|i| ((i * 37) % 2000 - 1000) as i16).collect();
        let wav = wav_bytes(&samples, SAMPLE_RATE);
        assert_eq!(wav.len(), 44 + 3200);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 36 + 3200);
        assert_eq!(decode_wav(&wav).unwrap(), samples);
    }

    #[test]
    fn stereo_and_other_rates_are_converted() {
        // 8 kHz stereo, one second: left = 1000, right = 3000.
        let mut wav = Vec::new();
        wav.extend(b"RIFF");
        wav.extend(0u32.to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(8000u32.to_le_bytes());
        wav.extend(32000u32.to_le_bytes());
        wav.extend(4u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend((8000u32 * 4).to_le_bytes());
        for _ in 0..8000 {
            wav.extend(1000i16.to_le_bytes());
            wav.extend(3000i16.to_le_bytes());
        }
        let samples = decode_wav(&wav).unwrap();
        assert_eq!(samples.len(), 16000);
        assert!(samples.iter().all(|&s| s == 2000));
    }

    #[test]
    fn bad_files_are_rejected() {
        assert!(decode_wav(b"nope").is_err());
        assert!(decode_wav(b"RIFF\0\0\0\0WAVE").is_err());
    }

    #[test]
    fn peak_is_relative_to_full_scale() {
        assert_eq!(peak(&[]), 0.0);
        assert_eq!(peak(&[0, -16384, 100]), 0.5);
    }
}

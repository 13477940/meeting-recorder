//! 16 kHz 單聲道 16-bit WAV 的讀寫（各平台共用）；從錄影檔抽音訊的實作在各平台的 audio_extract。

pub use crate::platform::audio_extract::extract_16k;

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

pub const SAMPLE_RATE: u32 = 16_000;

pub fn write_wav_header(f: &mut impl Write, data_bytes: u32) -> std::io::Result<()> {
    let byte_rate = SAMPLE_RATE * 2;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_bytes).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&1u16.to_le_bytes())?; // mono
    f.write_all(&SAMPLE_RATE.to_le_bytes())?;
    f.write_all(&byte_rate.to_le_bytes())?;
    f.write_all(&2u16.to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data_bytes.to_le_bytes())
}

/// 讀 extract_16k 產出的 WAV，轉成 whisper 要的 f32。
pub fn read_wav_f32(path: &Path) -> Result<Vec<f32>> {
    let mut f = File::open(path).with_context(|| format!("無法開啟 {}", path.display()))?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        bail!("{} 不是 WAV", path.display());
    }
    // 找 data chunk
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let size = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().unwrap()) as usize;
        if id == b"fmt " {
            let ch = u16::from_le_bytes([bytes[i + 10], bytes[i + 11]]);
            let rate = u32::from_le_bytes(bytes[i + 12..i + 16].try_into().unwrap());
            let bits = u16::from_le_bytes([bytes[i + 22], bytes[i + 23]]);
            if ch != 1 || rate != SAMPLE_RATE || bits != 16 {
                bail!("WAV 必須是 16 kHz 單聲道 16-bit（實際 {rate} Hz、{ch} 聲道、{bits} bit）");
            }
        }
        if id == b"data" {
            let end = (i + 8 + size).min(bytes.len());
            return Ok(bytes[i + 8..end]
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
                .collect());
        }
        i += 8 + size + (size % 2);
    }
    bail!("{} 找不到音訊資料", path.display())
}

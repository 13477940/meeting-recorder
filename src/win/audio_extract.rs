//! Windows：Media Foundation SourceReader 把每條音軌解碼並轉成 16 kHz 單聲道 16-bit，再依時間戳混音。

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use windows::Win32::Media::MediaFoundation::*;
use windows::core::HSTRING;

use crate::audio::{SAMPLE_RATE, write_wav_header};

/// SourceReader 要求 PCM 輸出時會自動插入解碼器與重新取樣（取樣率、聲道數轉換）
fn reader(input: &Path) -> Result<IMFSourceReader> {
    let url = HSTRING::from(input.as_os_str());
    Ok(unsafe { MFCreateSourceReaderFromURL(&url, None) }.with_context(|| format!("無法讀取 {}", input.display()))?)
}

fn audio_stream_indices(r: &IMFSourceReader) -> Vec<u32> {
    let mut v = Vec::new();
    for i in 0..16u32 {
        match unsafe { r.GetNativeMediaType(i, 0) } {
            Ok(t) => {
                if unsafe { t.GetGUID(&MF_MT_MAJOR_TYPE) }.is_ok_and(|g| g == MFMediaType_Audio) {
                    v.push(i);
                }
            }
            Err(_) => break,
        }
    }
    v
}

/// 讀一條音軌，回傳從 0 秒起算、16 kHz 的樣本（依時間戳補靜音對齊）
fn read_track(input: &Path, stream: u32) -> Result<Vec<i16>> {
    let r = reader(input)?;
    unsafe {
        r.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        r.SetStreamSelection(stream, true)?;
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 1)?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 2)?;
        t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, SAMPLE_RATE * 2)?;
        r.SetCurrentMediaType(stream, None, &t).context("無法設定 16 kHz 輸出格式")?;
    }
    let mut out: Vec<i16> = Vec::new();
    loop {
        let mut flags: u32 = 0;
        let mut ts: i64 = 0;
        let mut sample: Option<IMFSample> = None;
        unsafe { r.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample)) }?;
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            break;
        }
        let Some(s) = sample else { continue };
        let buf = unsafe { s.ConvertToContiguousBuffer() }?;
        let mut p: *mut u8 = std::ptr::null_mut();
        let mut len: u32 = 0;
        unsafe { buf.Lock(&mut p, None, Some(&mut len)) }?;
        let bytes = unsafe { std::slice::from_raw_parts(p, len as usize) };
        let pos = (ts.max(0) as i128 * SAMPLE_RATE as i128 / 10_000_000) as usize;
        let new: Vec<i16> = bytes.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        crate::audio::place_samples(&mut out, pos, &new);
        unsafe { buf.Unlock() }?;
    }
    Ok(out)
}

/// 回傳寫出的秒數
pub fn extract_16k(input: &Path, output: &Path) -> Result<f64> {
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.context("MFStartup 失敗")?;
    let streams = audio_stream_indices(&reader(input)?);
    if streams.is_empty() {
        bail!("{} 沒有音軌", input.display());
    }
    let mut mix: Vec<i32> = Vec::new();
    for s in streams {
        let t = read_track(input, s)?;
        if t.len() > mix.len() {
            mix.resize(t.len(), 0);
        }
        for (m, v) in mix.iter_mut().zip(t) {
            *m += v as i32;
        }
    }
    let tmp = output.with_extension("wav.part");
    let mut f = BufWriter::new(File::create(&tmp).with_context(|| format!("無法建立 {}", tmp.display()))?);
    let bytes = (mix.len() * 2) as u64;
    if bytes > u32::MAX as u64 - 36 {
        bail!("音檔超過 WAV 上限");
    }
    write_wav_header(&mut f, bytes as u32)?;
    for v in &mix {
        f.write_all(&(v.clamp(&(i16::MIN as i32), &(i16::MAX as i32)).to_owned() as i16).to_le_bytes())?;
    }
    f.flush()?;
    drop(f);
    std::fs::rename(&tmp, output)?;
    Ok(mix.len() as f64 / SAMPLE_RATE as f64)
}

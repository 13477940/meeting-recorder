//! 從錄影檔取出逐字稿用的音訊：所有音軌（系統聲音＋麥克風）混成 16 kHz 單聲道 16-bit WAV。
//! 由 AVAssetReaderAudioMixOutput 負責解碼、混音、重新取樣，錄影時不必多花運算；
//! 也能直接處理 OBS 錄的 .mov 和當機後殘留的 fragmented 檔。

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_av_foundation::{AVAssetReader, AVAssetReaderAudioMixOutput, AVAssetReaderStatus, AVMediaTypeAudio, AVURLAsset};
use objc2_avf_audio::{
    AVFormatIDKey, AVLinearPCMBitDepthKey, AVLinearPCMIsBigEndianKey, AVLinearPCMIsFloatKey,
    AVLinearPCMIsNonInterleaved, AVNumberOfChannelsKey, AVSampleRateKey,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};

pub const SAMPLE_RATE: u32 = 16_000;
const K_AUDIO_FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");

fn any<T: objc2::Message>(r: Retained<T>) -> Retained<AnyObject> {
    unsafe { Retained::cast_unchecked(r) }
}

fn write_wav_header(f: &mut impl Write, data_bytes: u32) -> std::io::Result<()> {
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

/// 回傳寫出的秒數。
pub fn extract_16k(input: &Path, output: &Path) -> Result<f64> {
    let url = NSURL::from_file_path(input).ok_or_else(|| anyhow!("路徑無法轉成 URL"))?;
    let asset = unsafe { AVURLAsset::URLAssetWithURL_options(&url, None) };
    #[allow(deprecated)]
    let tracks = unsafe { asset.tracksWithMediaType(AVMediaTypeAudio.unwrap()) };
    if tracks.count() == 0 {
        bail!("{} 沒有音軌", input.display());
    }
    let keys: [&NSString; 7] = unsafe {
        [
            AVFormatIDKey.unwrap(),
            AVSampleRateKey.unwrap(),
            AVNumberOfChannelsKey.unwrap(),
            AVLinearPCMBitDepthKey.unwrap(),
            AVLinearPCMIsFloatKey.unwrap(),
            AVLinearPCMIsBigEndianKey.unwrap(),
            AVLinearPCMIsNonInterleaved.unwrap(),
        ]
    };
    let vals = [
        any(NSNumber::new_u32(K_AUDIO_FORMAT_LINEAR_PCM)),
        any(NSNumber::new_f64(SAMPLE_RATE as f64)),
        any(NSNumber::new_u32(1)),
        any(NSNumber::new_u32(16)),
        any(NSNumber::new_bool(false)),
        any(NSNumber::new_bool(false)),
        any(NSNumber::new_bool(false)),
    ];
    let settings: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_retained_objects(&keys, &vals);

    let reader = unsafe { AVAssetReader::initWithAsset_error(AVAssetReader::alloc(), &asset) }
        .map_err(|e| anyhow!("無法讀取 {}：{}", input.display(), e.localizedDescription()))?;
    let out = unsafe { AVAssetReaderAudioMixOutput::assetReaderAudioMixOutputWithAudioTracks_audioSettings(&tracks, Some(&settings)) };
    unsafe {
        out.setAlwaysCopiesSampleData(false);
        if !reader.canAddOutput(&out) {
            bail!("無法加入音訊輸出");
        }
        reader.addOutput(&out);
        if !reader.startReading() {
            bail!("開始讀取失敗：{:?}", reader.error().map(|e| e.localizedDescription().to_string()));
        }
    }

    let tmp = output.with_extension("wav.part");
    let mut f = BufWriter::new(File::create(&tmp).with_context(|| format!("無法建立 {}", tmp.display()))?);
    write_wav_header(&mut f, 0)?;
    let mut total: u64 = 0;
    let mut buf = Vec::new();
    while let Some(sb) = unsafe { out.copyNextSampleBuffer() } {
        let Some(bb) = (unsafe { sb.data_buffer() }) else { continue };
        let len = unsafe { bb.data_length() };
        buf.resize(len, 0u8);
        let st = unsafe { bb.copy_data_bytes(0, len, std::ptr::NonNull::new(buf.as_mut_ptr().cast()).unwrap()) };
        if st != 0 {
            bail!("讀取音訊資料失敗：{st}");
        }
        f.write_all(&buf)?;
        total += len as u64;
    }
    if unsafe { reader.status() } != AVAssetReaderStatus::Completed {
        bail!("讀取未完成：{:?}", unsafe { reader.error() }.map(|e| e.localizedDescription().to_string()));
    }
    if total > u32::MAX as u64 - 36 {
        bail!("音檔超過 WAV 上限");
    }
    let mut f = f.into_inner()?;
    f.seek(SeekFrom::Start(0))?;
    write_wav_header(&mut f, total as u32)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, output)?;
    Ok(total as f64 / 2.0 / SAMPLE_RATE as f64)
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

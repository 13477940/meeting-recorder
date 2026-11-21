//! macOS：AVAssetReaderAudioMixOutput 把所有音軌（系統聲音＋麥克風）解碼、混音並轉成 16 kHz 單聲道。
//! 錄影時不必多花運算，也能處理 OBS 舊錄影和當機殘留的 fragmented 檔。

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
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


use crate::audio::{SAMPLE_RATE, write_wav_header};

const K_AUDIO_FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");

fn any<T: objc2::Message>(r: Retained<T>) -> Retained<AnyObject> {
    unsafe { Retained::cast_unchecked(r) }
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


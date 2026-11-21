//! AVAssetWriter 包裝：一條視訊軌（HEVC/H.264）＋最多兩條 AAC 音軌（系統聲音、麥克風），
//! 以 movieFragmentInterval 輸出 fragmented 檔案，當機時最多只少最後一個 fragment。

use std::path::Path;
use std::ptr::NonNull;
use std::sync::mpsc;

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, Message};
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterStatus, AVFileTypeMPEG4, AVFileTypeQuickTimeMovie,
    AVMediaTypeAudio, AVMediaTypeVideo, AVVideoAverageBitRateKey, AVVideoCodecKey,
    AVVideoCodecTypeH264, AVVideoCodecTypeHEVC, AVVideoCompressionPropertiesKey,
    AVVideoExpectedSourceFrameRateKey, AVVideoHeightKey, AVVideoMaxKeyFrameIntervalDurationKey,
    AVVideoWidthKey,
};
use objc2_avf_audio::{AVEncoderBitRateKey, AVFormatIDKey, AVNumberOfChannelsKey, AVSampleRateKey};
use objc2_core_media::{CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};

const K_AUDIO_FORMAT_MPEG4_AAC: u32 = u32::from_be_bytes(*b"aac ");

pub use crate::common::Codec;

#[derive(Clone, Debug)]
pub struct WriterConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: Codec,
    pub video_bitrate_kbps: u32,
    pub fragment_seconds: u32,
    pub system_audio: bool,
    pub mic: bool,
    /// true：.mov（QuickTime）；false：.mp4
    pub quicktime: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Track {
    Video,
    System,
    Mic,
}

pub struct Writer {
    w: Retained<AVAssetWriter>,
    video: Retained<AVAssetWriterInput>,
    system: Option<Retained<AVAssetWriterInput>>,
    mic: Option<Retained<AVAssetWriterInput>>,
    started: bool,
    /// 已結束的暫停區間（host 時鐘秒）
    pauses: Vec<(f64, f64)>,
    /// 進行中的暫停起點
    pause_open: Option<f64>,
    /// 各軌最後寫入的 pts（平移後），用來丟掉亂序的 buffer
    track_last: [f64; 3],
    last_pts: f64,
    pub counts: [u64; 3],
    pub dropped: [u64; 3],
}

// AVAssetWriter 本身可跨執行緒使用；這裡一律包在 Mutex 裡序列化存取。
unsafe impl Send for Writer {}

fn key(k: Option<&'static NSString>) -> &'static NSString {
    k.expect("AVFoundation 常數不存在")
}

fn dict(pairs: &[(&NSString, Retained<AnyObject>)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<&NSString> = pairs.iter().map(|(k, _)| *k).collect();
    let vals: Vec<Retained<AnyObject>> = pairs.iter().map(|(_, v)| v.clone()).collect();
    NSDictionary::from_retained_objects(&keys, &vals)
}

fn any<T: objc2::Message>(r: Retained<T>) -> Retained<AnyObject> {
    // SAFETY: 所有 Objective-C 物件都是 AnyObject
    unsafe { Retained::cast_unchecked(r) }
}
fn num_u(v: u32) -> Retained<AnyObject> {
    any(NSNumber::new_u32(v))
}
fn num_f(v: f64) -> Retained<AnyObject> {
    any(NSNumber::new_f64(v))
}

pub fn cm_seconds(t: CMTime) -> f64 {
    if t.timescale == 0 { 0.0 } else { t.value as f64 / t.timescale as f64 }
}

fn cm_from_seconds(s: f64, timescale: i32) -> CMTime {
    CMTime {
        value: (s * timescale as f64).round() as i64,
        timescale,
        flags: CMTimeFlags::Valid,
        epoch: 0,
    }
}

impl Writer {
    pub fn new(path: &Path, cfg: &WriterConfig) -> Result<Self> {
        if path.exists() {
            bail!("輸出檔已存在：{}", path.display());
        }
        let url = NSURL::from_file_path(path).ok_or_else(|| anyhow!("路徑無法轉成 URL"))?;
        let file_type = if cfg.quicktime {
            unsafe { AVFileTypeQuickTimeMovie }
        } else {
            unsafe { AVFileTypeMPEG4 }
        }
        .ok_or_else(|| anyhow!("找不到檔案類型常數"))?;
        let w = unsafe { AVAssetWriter::initWithURL_fileType_error(AVAssetWriter::alloc(), &url, file_type) }
            .map_err(|e| anyhow!("AVAssetWriter 建立失敗：{}", e.localizedDescription()))?;

        unsafe {
            w.setMovieFragmentInterval(cm_from_seconds(cfg.fragment_seconds as f64, 600));
            w.setInitialMovieFragmentInterval(cm_from_seconds(cfg.fragment_seconds as f64, 600));
            w.setMovieTimeScale(600);
        }

        // 視訊
        let codec = match cfg.codec {
            Codec::Hevc => unsafe { AVVideoCodecTypeHEVC },
            Codec::H264 => unsafe { AVVideoCodecTypeH264 },
        }
        .ok_or_else(|| anyhow!("找不到編碼常數"))?;
        let compression = unsafe {
            dict(&[
                (key(AVVideoAverageBitRateKey), num_u(cfg.video_bitrate_kbps * 1000)),
                (key(AVVideoExpectedSourceFrameRateKey), num_u(cfg.fps)),
                (key(AVVideoMaxKeyFrameIntervalDurationKey), num_f(10.0)),
            ])
        };
        let vsettings = unsafe {
            dict(&[
                (key(AVVideoCodecKey), any(codec.retain())),
                (key(AVVideoWidthKey), num_u(cfg.width)),
                (key(AVVideoHeightKey), num_u(cfg.height)),
                (key(AVVideoCompressionPropertiesKey), any(compression)),
            ])
        };
        let video = unsafe {
            AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
                AVMediaTypeVideo.unwrap(),
                Some(&vsettings),
            )
        };
        unsafe { video.setExpectsMediaDataInRealTime(true) };
        add_input(&w, &video, "視訊")?;

        let audio_input = |channels: u32, kbps: u32| -> Retained<AVAssetWriterInput> {
            let s = unsafe {
                dict(&[
                    (key(AVFormatIDKey), num_u(K_AUDIO_FORMAT_MPEG4_AAC)),
                    (key(AVSampleRateKey), num_f(48_000.0)),
                    (key(AVNumberOfChannelsKey), num_u(channels)),
                    (key(AVEncoderBitRateKey), num_u(kbps * 1000)),
                ])
            };
            let i = unsafe {
                AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(AVMediaTypeAudio.unwrap(), Some(&s))
            };
            unsafe { i.setExpectsMediaDataInRealTime(true) };
            i
        };
        let system = if cfg.system_audio {
            let i = audio_input(2, 96);
            add_input(&w, &i, "系統聲音")?;
            Some(i)
        } else {
            None
        };
        let mic = if cfg.mic {
            let i = audio_input(1, 64);
            add_input(&w, &i, "麥克風")?;
            Some(i)
        } else {
            None
        };

        if !unsafe { w.startWriting() } {
            bail!("startWriting 失敗：{}", writer_error(&w));
        }
        Ok(Self { w, video, system, mic, started: false, pauses: vec![], pause_open: None, track_last: [f64::MIN; 3], last_pts: 0.0, counts: [0; 3], dropped: [0; 3] })
    }

    /// 暫停：host 時鐘時間 `now` 之後的 buffer 一律丟掉，直到 resume。
    pub fn pause(&mut self, now: f64) {
        if self.pause_open.is_none() {
            self.pause_open = Some(now);
        }
    }

    /// 繼續：暫停區間記下來，之後的 buffer 往前平移區間長度，錄影檔不留空白。
    pub fn resume(&mut self, now: f64) {
        if let Some(s) = self.pause_open.take() {
            self.pauses.push((s, now));
        }
    }

    pub fn paused_seconds(&self) -> f64 {
        self.pauses.iter().map(|(s, e)| e - s).sum()
    }


    /// `raw` 為 CMSampleBufferRef（由 ScreenCaptureKit 交來，呼叫期間有效）。
    pub fn append(&mut self, track: Track, raw: *mut std::ffi::c_void) -> Result<()> {
        let idx = track as usize;
        let input = match track {
            Track::Video => &self.video,
            Track::System => match &self.system { Some(i) => i, None => return Ok(()) },
            Track::Mic => match &self.mic { Some(i) => i, None => return Ok(()) },
        };
        let sb: &CMSampleBuffer = unsafe { &*(raw as *const CMSampleBuffer) };
        let pts = unsafe { sb.presentation_time_stamp() };
        if !pts.flags.contains(CMTimeFlags::Valid) {
            self.dropped[idx] += 1;
            return Ok(());
        }

        let t = cm_seconds(pts);
        if self.pause_open.is_some_and(|s| t >= s) || self.pauses.iter().any(|&(s, e)| t >= s && t < e) {
            self.dropped[idx] += 1;
            return Ok(());
        }
        let offset: f64 = self.pauses.iter().filter(|&&(_, e)| e <= t).map(|(s, e)| e - s).sum();
        let retimed;
        let sb: &CMSampleBuffer = if offset > 0.0 {
            retimed = retime(sb, offset)?;
            &retimed
        } else {
            sb
        };
        let pts = unsafe { sb.presentation_time_stamp() };
        let t = cm_seconds(pts);
        if t <= self.track_last[idx] {
            self.dropped[idx] += 1;
            return Ok(());
        }

        if !self.started {
            unsafe { self.w.startSessionAtSourceTime(pts) };
            self.started = true;
        }
        if unsafe { self.w.status() } != AVAssetWriterStatus::Writing {
            bail!("writer 狀態異常：{}", writer_error(&self.w));
        }
        if !unsafe { input.isReadyForMoreMediaData() } {
            self.dropped[idx] += 1;
            return Ok(());
        }
        if unsafe { input.appendSampleBuffer(sb) } {
            self.counts[idx] += 1;
            self.track_last[idx] = t;
            self.last_pts = self.last_pts.max(t);
            Ok(())
        } else {
            bail!("append {:?} 失敗：{}", track, writer_error(&self.w))
        }
    }

    /// 收尾：標記結束並等待寫檔完成。
    pub fn finish(self) -> Result<()> {
        if !self.started {
            unsafe { self.w.cancelWriting() };
            bail!("沒有收到任何畫面或聲音，未產生檔案");
        }
        unsafe {
            self.w.endSessionAtSourceTime(cm_from_seconds(self.last_pts, 600));
            self.video.markAsFinished();
            if let Some(i) = &self.system { i.markAsFinished(); }
            if let Some(i) = &self.mic { i.markAsFinished(); }
        }
        let (tx, rx) = mpsc::channel::<()>();
        let block = RcBlock::new(move || {
            let _ = tx.send(());
        });
        unsafe { self.w.finishWritingWithCompletionHandler(&block) };
        rx.recv_timeout(std::time::Duration::from_secs(60)).context("等待寫檔完成逾時")?;
        if unsafe { self.w.status() } != AVAssetWriterStatus::Completed {
            bail!("寫檔未完成：{}", writer_error(&self.w));
        }
        Ok(())
    }
}

fn add_input(w: &AVAssetWriter, i: &AVAssetWriterInput, name: &str) -> Result<()> {
    if !unsafe { w.canAddInput(i) } {
        bail!("無法加入{}軌", name);
    }
    unsafe { w.addInput(i) };
    Ok(())
}

fn writer_error(w: &AVAssetWriter) -> String {
    unsafe { w.error() }
        .map(|e| format!("{} ({:?})", e.localizedDescription(), e.code()))
        .unwrap_or_else(|| format!("status={:?}", unsafe { w.status() }))
}

fn retime(sb: &CMSampleBuffer, offset: f64) -> Result<objc2_core_foundation::CFRetained<CMSampleBuffer>> {
    let mut needed: isize = 0;
    unsafe { sb.sample_timing_info_array(0, std::ptr::null_mut(), &mut needed) };
    let mut arr = vec![
        CMSampleTimingInfo { duration: CMTime { value: 0, timescale: 0, flags: CMTimeFlags(0), epoch: 0 },
            presentationTimeStamp: CMTime { value: 0, timescale: 0, flags: CMTimeFlags(0), epoch: 0 },
            decodeTimeStamp: CMTime { value: 0, timescale: 0, flags: CMTimeFlags(0), epoch: 0 } };
        needed.max(1) as usize
    ];
    let st = unsafe { sb.sample_timing_info_array(arr.len() as isize, arr.as_mut_ptr(), &mut needed) };
    if st != 0 {
        bail!("讀取時間資訊失敗：{st}");
    }
    for t in arr.iter_mut() {
        for c in [&mut t.presentationTimeStamp, &mut t.decodeTimeStamp] {
            if c.flags.contains(CMTimeFlags::Valid) {
                let ts = c.timescale;
                *c = cm_from_seconds(cm_seconds(*c) - offset, ts);
            }
        }
    }
    let mut out: *mut CMSampleBuffer = std::ptr::null_mut();
    let st = unsafe {
        CMSampleBuffer::create_copy_with_new_timing(None, sb, arr.len() as isize, arr.as_ptr(), NonNull::from(&mut out))
    };
    if st != 0 || out.is_null() {
        bail!("重設時間戳失敗：{st}");
    }
    Ok(unsafe { objc2_core_foundation::CFRetained::from_raw(NonNull::new_unchecked(out)) })
}

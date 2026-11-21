//! Media Foundation SinkWriter：一條視訊軌（H.264／HEVC，硬體編碼優先）＋最多兩條 AAC 音軌，
//! 容器用 fragmented MP4（MFTranscodeContainerType_FMPEG4），當機時已寫入的 fragment 仍可播放。

use std::path::Path;

use anyhow::{Context, Result, bail};
use windows::Win32::Media::MediaFoundation::*;
use windows::core::{GUID, HSTRING};

use crate::common::Codec;

pub struct SinkConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: Codec,
    pub video_bitrate_kbps: u32,
    pub fragment_seconds: u32,
    pub system_audio: bool,
    pub mic: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Track {
    Video,
    System,
    Mic,
}

/// 系統聲音雙聲道、麥克風單聲道，都是 48 kHz 16-bit PCM 輸入
pub const AUDIO_RATE: u32 = 48_000;
pub const SYSTEM_CHANNELS: u32 = 2;
pub const MIC_CHANNELS: u32 = 1;

pub struct Sink {
    w: IMFSinkWriter,
    video: u32,
    system: Option<u32>,
    mic: Option<u32>,
    width: u32,
    height: u32,
    frame_hns: i64,
    pub counts: [u64; 3],
    last_ts: [i64; 3],
}

// IMFSinkWriter 是 free-threaded COM 物件；這裡一律包在 Mutex 裡序列化存取。
unsafe impl Send for Sink {}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn media_type(major: &GUID, sub: &GUID) -> Result<IMFMediaType> {
    let t = unsafe { MFCreateMediaType() }?;
    unsafe {
        t.SetGUID(&MF_MT_MAJOR_TYPE, major)?;
        t.SetGUID(&MF_MT_SUBTYPE, sub)?;
    }
    Ok(t)
}

fn audio_types(channels: u32, aac_bytes_per_sec: u32) -> Result<(IMFMediaType, IMFMediaType)> {
    let out = media_type(&MFMediaType_Audio, &MFAudioFormat_AAC)?;
    let inp = media_type(&MFMediaType_Audio, &MFAudioFormat_PCM)?;
    unsafe {
        for t in [&out, &inp] {
            t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE)?;
            t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
        }
        // AAC 編碼器只接受 12000／16000／20000／24000 bytes/s
        out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, aac_bytes_per_sec)?;
        inp.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, channels * 2)?;
        inp.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_RATE * channels * 2)?;
    }
    Ok((out, inp))
}

impl Sink {
    pub fn new(path: &Path, cfg: &SinkConfig) -> Result<Self> {
        if path.exists() {
            bail!("輸出檔已存在：{}", path.display());
        }
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.context("MFStartup 失敗")?;

        let mut attrs: Option<IMFAttributes> = None;
        unsafe { MFCreateAttributes(&mut attrs, 4) }?;
        let attrs = attrs.context("MFCreateAttributes 沒有回傳物件")?;
        unsafe {
            attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_FMPEG4)?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
        }
        let url = HSTRING::from(path.as_os_str());
        let w = unsafe { MFCreateSinkWriterFromURL(&url, None, &attrs) }.context("建立 SinkWriter 失敗")?;

        // 視訊：輸出 H.264／HEVC，輸入 RGB32（BGRA，由上而下），色彩轉換交給 SinkWriter 內建的 video processor
        let sub = match cfg.codec {
            Codec::Hevc => MFVideoFormat_HEVC,
            Codec::H264 => MFVideoFormat_H264,
        };
        let out = media_type(&MFMediaType_Video, &sub)?;
        let inp = media_type(&MFMediaType_Video, &MFVideoFormat_RGB32)?;
        unsafe {
            for t in [&out, &inp] {
                t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                t.SetUINT64(&MF_MT_FRAME_SIZE, pack(cfg.width, cfg.height))?;
                t.SetUINT64(&MF_MT_FRAME_RATE, pack(cfg.fps, 1))?;
                t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            }
            out.SetUINT32(&MF_MT_AVG_BITRATE, cfg.video_bitrate_kbps * 1000)?;
            // fMP4 以關鍵影格切 fragment：關鍵影格間隔 = fragment 秒數
            out.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, cfg.fps * cfg.fragment_seconds.max(1))?;
            inp.SetUINT32(&MF_MT_DEFAULT_STRIDE, cfg.width * 4)?;
        }
        let video = unsafe { w.AddStream(&out) }.context("加入視訊軌失敗（這台電腦可能不支援所選編碼，可改用 H.264）")?;
        unsafe { w.SetInputMediaType(video, &inp, None) }.context("設定視訊輸入格式失敗")?;

        let add_audio = |channels: u32, bytes: u32, name: &str| -> Result<u32> {
            let (o, i) = audio_types(channels, bytes)?;
            let s = unsafe { w.AddStream(&o) }.with_context(|| format!("加入{name}軌失敗"))?;
            unsafe { w.SetInputMediaType(s, &i, None) }.with_context(|| format!("設定{name}輸入格式失敗"))?;
            Ok(s)
        };
        let system = if cfg.system_audio { Some(add_audio(SYSTEM_CHANNELS, 16_000, "系統聲音")?) } else { None };
        let mic = if cfg.mic { Some(add_audio(MIC_CHANNELS, 12_000, "麥克風")?) } else { None };

        unsafe { w.BeginWriting() }.context("BeginWriting 失敗")?;
        Ok(Self {
            w,
            video,
            system,
            mic,
            width: cfg.width,
            height: cfg.height,
            frame_hns: 10_000_000 / cfg.fps.max(1) as i64,
            counts: [0; 3],
            last_ts: [i64::MIN; 3],
        })
    }

    fn write(&mut self, track: Track, data: &[u8], ts: i64, dur: i64) -> Result<()> {
        let idx = track as usize;
        let stream = match track {
            Track::Video => self.video,
            Track::System => match self.system { Some(s) => s, None => return Ok(()) },
            Track::Mic => match self.mic { Some(s) => s, None => return Ok(()) },
        };
        if ts <= self.last_ts[idx] {
            return Ok(());
        }
        unsafe {
            let buf = MFCreateMemoryBuffer(data.len() as u32)?;
            let mut p: *mut u8 = std::ptr::null_mut();
            buf.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            buf.Unlock()?;
            buf.SetCurrentLength(data.len() as u32)?;
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            s.SetSampleTime(ts)?;
            s.SetSampleDuration(dur)?;
            self.w.WriteSample(stream, &s).with_context(|| format!("寫入 {track:?} 失敗"))?;
        }
        self.counts[idx] += 1;
        self.last_ts[idx] = ts;
        Ok(())
    }

    /// `bgra`：width×height×4，由上而下；`ts`：100 ns 單位
    pub fn write_video(&mut self, bgra: &[u8], ts: i64) -> Result<()> {
        debug_assert_eq!(bgra.len(), (self.width * self.height * 4) as usize);
        let dur = self.frame_hns;
        self.write(Track::Video, bgra, ts, dur)
    }

    /// `pcm`：16-bit interleaved，48 kHz
    pub fn write_audio(&mut self, track: Track, pcm: &[u8], ts: i64) -> Result<()> {
        let ch = if track == Track::System { SYSTEM_CHANNELS } else { MIC_CHANNELS } as i64;
        let frames = pcm.len() as i64 / (2 * ch);
        let dur = frames * 10_000_000 / AUDIO_RATE as i64;
        self.write(track, pcm, ts, dur)
    }

    pub fn finish(self) -> Result<()> {
        if self.counts.iter().all(|&c| c == 0) {
            bail!("沒有收到任何畫面或聲音");
        }
        unsafe { self.w.Finalize() }.context("Finalize 失敗")?;
        Ok(())
    }
}

/// 把擷取到的 BGRA 畫面（含裁切範圍）縮放成輸出尺寸；雙線性內插。
/// 每秒只處理幾張，CPU 成本可以接受。
pub fn scale_bgra(src: &[u8], sw: u32, sh: u32, crop: (u32, u32, u32, u32), dw: u32, dh: u32, out: &mut Vec<u8>) {
    let (cx, cy, cw, ch) = crop;
    out.resize((dw * dh * 4) as usize, 0);
    let fx = cw as f32 / dw as f32;
    let fy = ch as f32 / dh as f32;
    let at = |x: u32, y: u32, c: usize| -> f32 {
        let x = x.min(sw - 1);
        let y = y.min(sh - 1);
        src[((y * sw + x) * 4) as usize + c] as f32
    };
    for y in 0..dh {
        let sy = cy as f32 + (y as f32 + 0.5) * fy - 0.5;
        let y0 = sy.floor().max(0.0) as u32;
        let wy = (sy - y0 as f32).clamp(0.0, 1.0);
        for x in 0..dw {
            let sx = cx as f32 + (x as f32 + 0.5) * fx - 0.5;
            let x0 = sx.floor().max(0.0) as u32;
            let wx = (sx - x0 as f32).clamp(0.0, 1.0);
            let o = ((y * dw + x) * 4) as usize;
            for c in 0..3 {
                let top = at(x0, y0, c) * (1.0 - wx) + at(x0 + 1, y0, c) * wx;
                let bot = at(x0, y0 + 1, c) * (1.0 - wx) + at(x0 + 1, y0 + 1, c) * wx;
                out[o + c] = (top * (1.0 - wy) + bot * wy).round() as u8;
            }
            out[o + 3] = 255;
        }
    }
}

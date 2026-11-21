//! <app_support>/config.toml（macOS：~/Library/Application Support/MeetingRecorder；Windows：%APPDATA%\MeetingRecorder）。
//! API key 不放這裡：macOS 放 Keychain，Windows 放認證管理員。

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::transcript::app_support;
use crate::common::Codec;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub capture: Capture,
    pub encode: Encode,
    pub output: Output,
    pub transcribe: Transcribe,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Capture {
    /// display | window | region
    pub target: String,
    /// target = window 時比對的 App 名稱
    pub app: String,
    /// target = region 時的 x,y,w,h（點）
    pub region: [f64; 4],
    pub fps: u32,
    /// 720 | 1080
    pub resolution: u32,
    pub show_cursor: bool,
    pub capture_system_audio: bool,
    pub capture_mic: bool,
    /// "default" 或 `sources` 列出的麥克風 id
    pub mic_device: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Encode {
    pub video_codec: Codec,
    pub video_bitrate_kbps: u32,
    pub fragment_seconds: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Output {
    pub root: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Transcribe {
    pub local: Local,
    pub gemini: Gemini,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Local {
    pub model: String,
    pub language: String,
    pub vad: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Gemini {
    /// 依 Gemini 官方模型頁填入；2026-11-22 查證 gemini-3.8-flash 為 stable、支援音訊輸入
    pub model: String,
    pub chunk_minutes: u32,
    pub overlap_seconds: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self { capture: Capture::default(), encode: Encode::default(), output: Output::default(), transcribe: Transcribe::default() }
    }
}
impl Default for Capture {
    fn default() -> Self {
        Self {
            target: "display".into(),
            app: String::new(),
            region: [0.0, 0.0, 1280.0, 720.0],
            fps: 2,
            resolution: 720,
            show_cursor: true,
            capture_system_audio: true,
            capture_mic: true,
            mic_device: "default".into(),
        }
    }
}
impl Default for Encode {
    fn default() -> Self {
        Self { video_codec: Codec::Hevc, video_bitrate_kbps: 400, fragment_seconds: 10 }
    }
}
impl Default for Output {
    fn default() -> Self {
        Self { root: crate::common::DEFAULT_OUTPUT_ROOT.into() }
    }
}
impl Default for Local {
    fn default() -> Self {
        Self { model: "large-v3-turbo".into(), language: "zh".into(), vad: true }
    }
}
impl Default for Gemini {
    fn default() -> Self {
        Self { model: "gemini-3.8-flash".into(), chunk_minutes: 10, overlap_seconds: 15 }
    }
}

pub fn path() -> PathBuf {
    app_support().join("config.toml")
}

pub fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => crate::common::home_dir().join(rest),
        None => PathBuf::from(p),
    }
}

impl Config {
    /// 沒有設定檔就寫一份預設值
    pub fn load() -> Result<Self> {
        let p = path();
        if !p.exists() {
            let c = Config::default();
            c.save()?;
            return Ok(c);
        }
        let s = std::fs::read_to_string(&p)?;
        toml::from_str(&s).with_context(|| format!("設定檔格式錯誤：{}", p.display()))
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(app_support())?;
        std::fs::write(path(), toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn output_root(&self) -> PathBuf {
        expand(&self.output.root)
    }
}

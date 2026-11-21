//! 各平台共用的錄影型別與小工具。

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::session::{Session, Status};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    Hevc,
    H264,
}

/// 開始錄影前至少要有的剩餘空間
pub const MIN_FREE_TO_START: u64 = 2 * 1024 * 1024 * 1024;
/// 錄影中低於此值就自動停止並存檔
pub const MIN_FREE_WHILE_RECORDING: u64 = 500 * 1024 * 1024;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Target {
    /// display_id；None 表示主螢幕
    Display { id: Option<u32> },
    /// 指定 App 的最前面一個視窗（名稱、bundle id 或執行檔名稱部分比對）
    App { name: String },
    /// 主螢幕上的一塊區域（點座標）
    Region { x: f64, y: f64, w: f64, h: f64 },
}

impl Target {
    pub fn describe(&self) -> String {
        match self {
            Target::Display { id } => format!("display:{}", id.map(|i| i.to_string()).unwrap_or("main".into())),
            Target::App { name } => format!("app:{name}"),
            Target::Region { x, y, w, h } => format!("region:{x},{y},{w},{h}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RecordOptions {
    pub name: String,
    pub root: PathBuf,
    pub target: Target,
    pub fps: u32,
    /// 720 或 1080：短邊目標
    pub resolution: u32,
    pub codec: Codec,
    pub video_bitrate_kbps: u32,
    pub fragment_seconds: u32,
    pub show_cursor: bool,
    pub system_audio: bool,
    pub mic: bool,
    pub mic_device: Option<String>,
}

fn even(v: f64) -> u32 {
    let v = v.round() as u32;
    v + (v % 2)
}

/// 依擷取來源長寬比算輸出尺寸：短邊對齊 720/1080，寬度不小於 16:9 時的寬度。
pub fn output_size(src_w: f64, src_h: f64, resolution: u32) -> (u32, u32) {
    let aspect = src_w / src_h;
    let min_w = resolution as f64 * 16.0 / 9.0;
    let (mut w, mut h) = (resolution as f64 * aspect, resolution as f64);
    if w < min_w {
        w = min_w;
        h = min_w / aspect;
    }
    // 不要放大超過來源
    if w > src_w * 2.0 {
        w = src_w * 2.0;
        h = src_h * 2.0;
    }
    (even(w), even(h))
}

pub fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if "/\\:*?\"<>|".contains(c) || c.is_control() { '_' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

pub fn dir_name(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// 使用者家目錄（macOS 的 HOME、Windows 的 USERPROFILE）
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// 設定、模型、詞彙表的存放處
pub fn app_support() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home_dir().join("Library/Application Support/MeetingRecorder")
    }
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(home_dir).join("MeetingRecorder")
    }
}

/// 預設的錄影輸出根目錄（設定檔裡的寫法，~ 代表家目錄）
pub const DEFAULT_OUTPUT_ROOT: &str = if cfg!(windows) { "~/Videos/MeetingRecorder" } else { "~/Movies/MeetingRecorder" };

/// 用系統預設方式開啟資料夾或檔案
pub fn open_path(p: &Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(p).status();
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer.exe").arg(p).status();
}

/// 用純文字編輯器開啟（macOS 文字編輯、Windows 記事本）
pub fn open_text(p: &Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg("-e").arg(p).status();
    #[cfg(windows)]
    let _ = std::process::Command::new("notepad.exe").arg(p).status();
}

/// 建立場次資料夾：<根目錄>/<YYYY-MM-DD_HHMM>[_<會議名稱>][_n]
pub fn new_session_dir(opts: &RecordOptions) -> Result<(PathBuf, chrono::DateTime<chrono::Local>)> {
    let now = chrono::Local::now();
    let base = if opts.name.trim().is_empty() {
        now.format("%Y-%m-%d_%H%M").to_string()
    } else {
        format!("{}_{}", now.format("%Y-%m-%d_%H%M"), sanitize(&opts.name))
    };
    let mut dir = opts.root.join(&base);
    let mut n = 2;
    while dir.exists() {
        dir = opts.root.join(format!("{base}_{n}"));
        n += 1;
    }
    std::fs::create_dir_all(&dir)?;
    Ok((dir, now))
}

pub fn new_session(opts: &RecordOptions, now: chrono::DateTime<chrono::Local>, file_name: &str, width: u32, height: u32) -> Session {
    Session {
        app_version: env!("CARGO_PKG_VERSION").into(),
        name: opts.name.clone(),
        status: Status::Recording,
        started_at: now.to_rfc3339(),
        ended_at: None,
        recording_file: file_name.into(),
        target: opts.target.describe(),
        width,
        height,
        fps: opts.fps,
        codec: format!("{:?}", opts.codec).to_lowercase(),
        video_bitrate_kbps: opts.video_bitrate_kbps,
        fragment_seconds: opts.fragment_seconds,
        system_audio: opts.system_audio,
        mic: opts.mic,
        mic_device: opts.mic_device.clone(),
        paused_seconds: 0.0,
        duration_seconds: None,
        frames: None,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn size() {
        assert_eq!(output_size(1920.0, 1080.0, 720), (1280, 720));
        assert_eq!(output_size(1512.0, 982.0, 720), (1280, 832));
        assert_eq!(output_size(3840.0, 2160.0, 1080), (1920, 1080));
    }
}

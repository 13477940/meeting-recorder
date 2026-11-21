//! session.json：開始／結束時間、設定、狀態。狀態停在 recording 代表上一場沒有正常結束。

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Recording,
    Paused,
    Finished,
    Failed,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Session {
    pub app_version: String,
    pub name: String,
    pub status: Status,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub recording_file: String,
    pub target: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: String,
    pub video_bitrate_kbps: u32,
    pub fragment_seconds: u32,
    pub system_audio: bool,
    pub mic: bool,
    pub mic_device: Option<String>,
    pub paused_seconds: f64,
    pub duration_seconds: Option<f64>,
    pub frames: Option<[u64; 3]>,
    pub error: Option<String>,
}

impl Session {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join("session.json")
    }

    /// 先寫暫存檔再 rename，避免當機時留下半份 JSON。
    pub fn save(&self, dir: &Path) -> Result<()> {
        let tmp = dir.join(".session.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, Self::path(dir))?;
        Ok(())
    }

    pub fn load(dir: &Path) -> Result<Self> {
        Ok(serde_json::from_slice(&std::fs::read(Self::path(dir))?)?)
    }
}

/// 列出輸出根目錄下狀態仍是 recording／paused 的場次（上次沒有正常結束）。
pub fn find_unfinished(root: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(root) else { return vec![] };
    rd.filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            Session::load(p)
                .map(|s| matches!(s.status, Status::Recording | Status::Paused))
                .unwrap_or(false)
        })
        .collect()
}

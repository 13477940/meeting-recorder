//! 會後處理工作（CLI 與選單列 App 共用）：取音訊 → 逐字稿 → 紀錄草稿。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, anyhow, bail};

use crate::config::Config;
use crate::transcript::{self, Transcript};
use crate::{audio, gemini, local_asr, recorder, session};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Local,
    Gemini,
}

/// 場次資料夾：用裡面的錄影檔；單一檔案：輸出到 out（預設和檔案同一層的 <檔名>_transcript/）
pub fn resolve_input(input: &Path, out: Option<PathBuf>) -> Result<(PathBuf, PathBuf, String)> {
    if input.is_dir() {
        let s = session::Session::load(input).ok();
        let file = s.as_ref().map(|s| s.recording_file.clone()).unwrap_or_else(|| "recording.mov".into());
        let title = s.map(|s| s.name).filter(|n| !n.is_empty()).unwrap_or_else(|| recorder::dir_name(input));
        Ok((input.join(file), out.unwrap_or_else(|| input.to_path_buf()), title))
    } else if input.is_file() {
        let stem = input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let out = out.unwrap_or_else(|| input.with_file_name(format!("{stem}_transcript")));
        Ok((input.to_path_buf(), out, stem))
    } else {
        bail!("找不到 {}", input.display())
    }
}

/// 回傳 transcript.md 路徑。`progress` 收到的是給人看的進度文字。
pub fn transcribe(
    input: &Path,
    out: Option<PathBuf>,
    engine: Engine,
    retry_failed: bool,
    cfg: &Config,
    cancel: Arc<AtomicBool>,
    progress: impl FnMut(String) + Send + 'static,
) -> Result<(PathBuf, Transcript)> {
    let (media, dir, title) = resolve_input(input, out)?;
    std::fs::create_dir_all(&dir)?;
    let wav = dir.join("audio_16k.wav");
    let progress = Arc::new(std::sync::Mutex::new(progress));
    if !wav.exists() {
        (progress.lock().unwrap())("取出音訊".into());
        audio::extract_16k(&media, &wav)?;
    }
    let t = match engine {
        Engine::Local => {
            let p = progress.clone();
            local_asr::transcribe(&wav, &cfg.transcribe.local.model, cfg.transcribe.local.vad, cancel, move |pct| {
                (p.lock().unwrap())(format!("本機辨識 {pct}%"))
            })?
        }
        Engine::Gemini => {
            let g = gemini::Gemini::new(&cfg.transcribe.gemini.model)?;
            let prev = if retry_failed {
                let p: Transcript = serde_json::from_slice(&std::fs::read(dir.join("transcript.gemini.json"))?)?;
                if p.failed_chunks.is_empty() {
                    bail!("上次沒有失敗的段落");
                }
                Some(p)
            } else {
                None
            };
            let p = progress.clone();
            g.transcribe(&wav, cfg.transcribe.gemini.chunk_minutes, cfg.transcribe.gemini.overlap_seconds, prev.as_ref(), move |i, n| {
                (p.lock().unwrap())(format!("Gemini 第 {i}/{n} 段"))
            })?
        }
    };
    transcript::write_outputs(&dir, &t, &title)?;
    Ok((dir.join("transcript.md"), t))
}

/// 由 transcript.md 產生 minutes_draft.md；回傳 (路徑, 沒有時間碼的條目數)
pub fn minutes(dir: &Path, cfg: &Config) -> Result<(PathBuf, usize)> {
    let md = std::fs::read_to_string(dir.join("transcript.md"))
        .map_err(|_| anyhow!("{} 沒有 transcript.md，請先產生逐字稿", dir.display()))?;
    let g = gemini::Gemini::new(&cfg.transcribe.gemini.model)?;
    let draft = g.minutes(&md)?;
    let missing = gemini::lines_missing_timecode(&draft);
    let mut out = format!("> 草稿由 {} 依 transcript.md 產生，每條內容請對照〔時間碼〕人工核對後再使用。\n\n{draft}\n", g.model);
    if !missing.is_empty() {
        out.push_str("\n---\n\n> 以下條目沒有時間碼，依規則不應採用，請刪除或補依據：\n\n");
        for m in &missing {
            out.push_str(&format!("> {m}\n"));
        }
    }
    let p = dir.join("minutes_draft.md");
    std::fs::write(&p, out)?;
    Ok((p, missing.len()))
}

/// 輸出根目錄下最近的場次（依資料夾名稱，新到舊）
pub fn recent_sessions(root: &Path, n: usize) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(root) else { return vec![] };
    let mut v: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join("session.json").exists())
        .collect();
    v.sort();
    v.reverse();
    v.truncate(n);
    v
}

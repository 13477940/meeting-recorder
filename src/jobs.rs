//! 會後處理工作（CLI 與選單列 App 共用）：取音訊 → 逐字稿 → 紀錄草稿。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow, bail};

use crate::config::Config;
use crate::transcript::{self, Transcript};
use crate::{audio, common, gemini, local_asr, session};

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
        let title = s.map(|s| s.name).filter(|n| !n.is_empty()).unwrap_or_else(|| common::dir_name(input));
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
    // 取音訊本身無法中斷（通常幾秒），結束後再檢查一次
    if cancel.load(Ordering::Relaxed) {
        bail!("已取消");
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
            g.transcribe(&wav, cfg.transcribe.gemini.chunk_minutes, cfg.transcribe.gemini.overlap_seconds, prev.as_ref(), &cancel, move |i, n| {
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

/// 選單列 App 的背景工作狀態（一次只跑一件）
#[derive(Default)]
pub struct Job {
    /// 正在做什麼（None = 沒有工作）
    pub running: Option<String>,
    pub progress: String,
    /// 工作結束後要顯示的訊息：(標題, 內容, 要開的資料夾)
    pub notice: Option<(String, String, Option<PathBuf>)>,
    pub cancel: Arc<AtomicBool>,
    /// 這件工作能不能取消（會議紀錄草稿是單一請求，不能）
    pub cancellable: bool,
}

/// 在背景執行緒產生逐字稿；已有工作在跑就回傳 false
pub fn spawn_transcribe(job: Arc<std::sync::Mutex<Job>>, dir: PathBuf, engine: Engine, retry: bool, cfg: Config) -> bool {
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut j = job.lock().unwrap();
        if j.running.is_some() {
            return false;
        }
        j.running = Some(format!("{}逐字稿", if engine == Engine::Local { "本機" } else { "Gemini " }));
        j.progress = "準備中".into();
        j.cancel = cancel.clone();
        j.cancellable = true;
    }
    std::thread::spawn(move || {
        let j2 = job.clone();
        let cancelled = cancel.clone();
        let res = transcribe(&dir, None, engine, retry, &cfg, cancel, move |p| {
            j2.lock().unwrap().progress = p;
        });
        let mut j = job.lock().unwrap();
        j.running = None;
        j.notice = Some(match res {
            Ok((md, t)) => {
                let mut body = format!(
                    "{} 段，音訊 {:.1} 分，耗時 {:.1} 分\n{}",
                    t.segments.len(),
                    t.audio_seconds / 60.0,
                    t.elapsed_seconds / 60.0,
                    md.display()
                );
                if !t.failed_chunks.is_empty() {
                    body.push_str(&format!("\n有 {} 段失敗，可在「最近 5 場」裡重跑。", t.failed_chunks.len()));
                }
                ("逐字稿完成".into(), body, Some(dir))
            }
            Err(_) if cancelled.load(Ordering::Relaxed) => ("逐字稿已取消".into(), "已停止處理。".into(), None),
            Err(e) => ("逐字稿失敗".into(), format!("{e:#}"), None),
        });
    });
    true
}

/// 在背景執行緒產生會議紀錄草稿；已有工作在跑就回傳 false
pub fn spawn_minutes(job: Arc<std::sync::Mutex<Job>>, dir: PathBuf, cfg: Config) -> bool {
    {
        let mut j = job.lock().unwrap();
        if j.running.is_some() {
            return false;
        }
        j.running = Some("會議紀錄草稿".into());
        j.progress = "Gemini 處理中".into();
        j.cancellable = false;
    }
    std::thread::spawn(move || {
        let res = minutes(&dir, &cfg);
        let mut j = job.lock().unwrap();
        j.running = None;
        j.notice = Some(match res {
            Ok((p, missing)) => {
                let extra = if missing > 0 { format!("\n有 {missing} 條沒有時間碼，已列在草稿最後，請刪除或補依據。") } else { String::new() };
                ("會議紀錄草稿完成".into(), format!("{}{extra}", p.display()), Some(dir))
            }
            Err(e) => ("會議紀錄草稿失敗".into(), format!("{e:#}"), None),
        });
    });
    true
}

/// 這一場上次的 Gemini 逐字稿有沒有失敗段落
pub fn has_failed_chunks(dir: &Path) -> bool {
    std::fs::read(dir.join("transcript.gemini.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Transcript>(&b).ok())
        .is_some_and(|t| !t.failed_chunks.is_empty())
}

/// 啟動時找出上次沒有正常結束的場次，標成 failed 後回傳（讓 UI 提示使用者檢查）
pub fn mark_unfinished(root: &Path) -> Vec<PathBuf> {
    let v = session::find_unfinished(root);
    for d in &v {
        if let Ok(mut s) = session::Session::load(d) {
            s.status = session::Status::Failed;
            s.error = Some("上次錄影沒有正常結束（App 當掉或被強制結束）；已錄到的 fragment 仍可播放".into());
            let _ = s.save(d);
        }
    }
    v
}

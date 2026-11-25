//! 本機逐字稿：whisper.cpp（whisper-rs，Metal）＋ Silero VAD。不分講者。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, anyhow, bail};
#[cfg(feature = "local-asr")]
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperVadParams};

use crate::transcript::{Transcript, app_support};

pub struct ModelInfo {
    pub name: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: u64,
}

/// SHA-256 取自 Hugging Face 檔案清單（lfs.oid），2026-11-22 查證
pub const MODELS: &[ModelInfo] = &[
    ModelInfo {
        name: "large-v3-turbo",
        file: "ggml-large-v3-turbo.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin",
        sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
        size: 1_624_555_275,
    },
    ModelInfo {
        name: "medium",
        file: "ggml-medium.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.bin",
        sha256: "6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208",
        size: 1_533_763_059,
    },
    ModelInfo {
        name: "silero-vad",
        file: "ggml-silero-v6.2.0.bin",
        url: "https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v6.2.0.bin",
        sha256: "2aa269b785eeb53a82983a20501ddf7c1d9c48e33ab63a41391ac6c9f7fb6987",
        size: 885_098,
    },
];

pub fn models_dir() -> PathBuf {
    app_support().join("models")
}

pub fn model(name: &str) -> Result<&'static ModelInfo> {
    MODELS.iter().find(|m| m.name == name).ok_or_else(|| anyhow!("不認得的模型：{name}"))
}

pub fn model_path(name: &str) -> Result<PathBuf> {
    let m = model(name)?;
    let p = models_dir().join(m.file);
    if !p.exists() {
        bail!("模型 {} 尚未下載，請先執行：meeting-recorder models download {}", m.name, m.name);
    }
    Ok(p)
}

#[cfg(feature = "local-asr")]
/// whisper 的 initial prompt 有長度上限（約 224 token），中文一字約 1～2 token，取前面的詞到約 120 字。
fn build_prompt(glossary: &[String]) -> String {
    let mut s = String::from("以下是臺灣的會議逐字稿，使用繁體中文。");
    let mut terms = Vec::new();
    let mut len = 0;
    for g in glossary {
        len += g.chars().count() + 1;
        if len > 120 {
            break;
        }
        terms.push(g.as_str());
    }
    if !terms.is_empty() {
        s.push_str("詞彙：");
        s.push_str(&terms.join("、"));
        s.push('。');
    }
    s
}

#[cfg(not(feature = "local-asr"))]
pub fn transcribe(_wav: &Path, _model: &str, _vad: bool, _cancel: Arc<AtomicBool>, _progress: impl FnMut(i32) + 'static) -> Result<Transcript> {
    bail!("這個版本編譯時沒有開啟本機辨識（feature local-asr）")
}

#[cfg(feature = "local-asr")]
pub fn transcribe(
    wav: &Path,
    model_name: &str,
    vad: bool,
    cancel: Arc<AtomicBool>,
    mut progress: impl FnMut(i32) + 'static,
) -> Result<Transcript> {
    use crate::transcript::{PostProcess, Segment, load_glossary};
    use std::sync::atomic::Ordering;
    let samples = crate::audio::read_wav_f32(wav)?;
    let audio_seconds = samples.len() as f64 / crate::audio::SAMPLE_RATE as f64;
    let mpath = model_path(model_name)?;
    let started = std::time::Instant::now();

    let mut cp = WhisperContextParameters::default();
    cp.use_gpu(true).flash_attn(true);
    let ctx = WhisperContext::new_with_params(mpath.to_str().unwrap(), cp)
        .map_err(|e| anyhow!("載入模型失敗：{e:?}"))?;
    let mut state = ctx.create_state().map_err(|e| anyhow!("建立 state 失敗：{e:?}"))?;

    let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    p.set_language(Some("zh"));
    p.set_n_threads(std::thread::available_parallelism().map(|n| n.get().min(8) as i32).unwrap_or(4));
    p.set_print_special(false);
    p.set_print_progress(false);
    p.set_print_realtime(false);
    p.set_print_timestamps(false);
    let prompt = build_prompt(&load_glossary());
    p.set_initial_prompt(&prompt);
    let vad_path;
    if vad {
        vad_path = model_path("silero-vad")?;
        p.set_vad_model_path(Some(vad_path.to_str().unwrap()));
        p.enable_vad(true);
        p.set_vad_params(WhisperVadParams::default());
    }
    // 不用 set_progress_callback_safe：它用 Box::into_raw 交出閉包後永遠不釋放，每跑一次就洩漏一份
    //（連同閉包抓住的 Arc<Mutex<Job>>）。改成閉包留在這個函式的堆疊上，C 端只拿指標。
    unsafe extern "C" fn progress_cb(
        _ctx: *mut whisper_rs::WhisperSysContext,
        _state: *mut whisper_rs::WhisperSysState,
        pct: std::os::raw::c_int,
        data: *mut std::ffi::c_void,
    ) {
        let f = unsafe { &mut *(data as *mut &mut dyn FnMut(i32)) };
        // panic 不能穿過 extern "C" 邊界（會直接中止整個程式），在這裡吞掉；進度回報失敗不影響辨識
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(pct)));
    }
    let mut progress_dyn: &mut dyn FnMut(i32) = &mut progress;
    // SAFETY: progress_dyn 在 state.full 回傳前都還在這個堆疊框裡
    unsafe {
        p.set_progress_callback(Some(progress_cb));
        p.set_progress_callback_user_data(&mut progress_dyn as *mut &mut dyn FnMut(i32) as *mut std::ffi::c_void);
    }
    // 不用 whisper-rs 0.16 的 set_abort_callback_safe：它的 trampoline 把 Box<dyn FnMut> 當成閉包本身解讀，
    // 會讀到錯誤的記憶體而隨機回傳 true，造成「failed to encode」。改成直接把 AtomicBool 的指標交給 C 端。
    unsafe extern "C" fn abort_cb(data: *mut std::ffi::c_void) -> bool {
        unsafe { (*(data as *const std::sync::atomic::AtomicBool)).load(Ordering::Relaxed) }
    }
    // SAFETY: cancel（Arc）在 state.full 回傳前都還活著，指標在整個辨識期間有效
    unsafe {
        p.set_abort_callback(Some(abort_cb));
        p.set_abort_callback_user_data(Arc::as_ptr(&cancel) as *mut std::ffi::c_void);
    }

    let res = state.full(p, &samples);
    if cancel.load(Ordering::Relaxed) {
        bail!("{}", crate::gemini::CANCELLED);
    }
    res.map_err(|e| anyhow!("辨識失敗：{e:?}"))?;

    let pp = PostProcess::new()?;
    let mut segments = Vec::new();
    for seg in state.as_iter() {
        let text = seg.to_str_lossy().map(|s| s.into_owned()).unwrap_or_default();
        let text = pp.apply(&text);
        if text.is_empty() {
            continue;
        }
        segments.push(Segment {
            // whisper 的時間單位是 10 ms
            start: seg.start_timestamp() as f64 / 100.0,
            end: seg.end_timestamp() as f64 / 100.0,
            speaker: String::new(),
            text,
        });
    }
    Ok(Transcript {
        engine: "local".into(),
        model: model_name.into(),
        created_at: chrono::Local::now().to_rfc3339(),
        audio_seconds,
        elapsed_seconds: started.elapsed().as_secs_f64(),
        segments,
        failed_chunks: vec![],
        chunk_plan: None,
    })
}

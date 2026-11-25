//! 逐字稿資料結構、後處理（簡轉繁、誤辨對照）、輸出 md／srt／json。

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Segment {
    /// 秒
    pub start: f64,
    pub end: f64,
    #[serde(default)]
    pub speaker: String,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Transcript {
    pub engine: String,
    pub model: String,
    pub created_at: String,
    pub audio_seconds: f64,
    pub elapsed_seconds: f64,
    pub segments: Vec<Segment>,
    /// Gemini 路徑中失敗的段落（起始秒），可單獨重跑
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_chunks: Vec<f64>,
}

pub use crate::common::app_support;

const DEFAULT_GLOSSARY: &str = "\
# 專案詞彙表：一行一個詞，會放進辨識提示，# 開頭為註解
# 例：
# 單一登入
# Kubernetes
";

const DEFAULT_CORRECTIONS: &str = "\
# 常見誤辨 → 正確詞，一行一組，用 → 或 => 分隔；# 開頭為註解
# 例：中街 → 中介
";

/// 詞彙表：一行一個詞
pub fn load_glossary() -> Vec<String> {
    let p = app_support().join("glossary.txt");
    if !p.exists() {
        let _ = std::fs::create_dir_all(app_support());
        let _ = std::fs::write(&p, DEFAULT_GLOSSARY);
    }
    std::fs::read_to_string(&p)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// 誤辨對照表
pub fn load_corrections() -> Vec<(String, String)> {
    let p = app_support().join("corrections.txt");
    if !p.exists() {
        let _ = std::fs::create_dir_all(app_support());
        let _ = std::fs::write(&p, DEFAULT_CORRECTIONS);
    }
    std::fs::read_to_string(&p)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let (a, b) = l.split_once('→').or_else(|| l.split_once("=>"))?;
            let (a, b) = (a.trim(), b.trim());
            (!a.is_empty()).then(|| (a.to_string(), b.to_string()))
        })
        .collect()
}

pub struct PostProcess {
    cc: ferrous_opencc::OpenCC,
    corrections: Vec<(String, String)>,
}

impl PostProcess {
    pub fn new() -> Result<Self> {
        let cc = ferrous_opencc::OpenCC::from_config(ferrous_opencc::config::BuiltinConfig::S2twp)
            .map_err(|e| anyhow::anyhow!("OpenCC 初始化失敗：{e}"))?;
        Ok(Self { cc, corrections: load_corrections() })
    }

    pub fn apply(&self, text: &str) -> String {
        let mut s = self.cc.convert(text.trim());
        for (a, b) in &self.corrections {
            s = s.replace(a.as_str(), b);
        }
        fullwidth_punct(&s)
    }
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x9FFF | 0xF900..=0xFAFF | 0x3000..=0x303F | 0xFF00..=0xFFEF)
}

/// 中文字旁的半形標點改成全形（whisper 常輸出半形逗號、問號）
pub fn fullwidth_punct(s: &str) -> String {
    // 刪節號先整組換掉，避免第一個點被轉成句號（「。..」）
    let s = s.replace("。。。", "……").replace("...", "……");
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        let prev_cjk = i > 0 && is_cjk(chars[i - 1]);
        let next_cjk = chars.get(i + 1).is_some_and(|&n| is_cjk(n));
        let full = match c {
            ',' => Some('，'),
            '?' => Some('？'),
            '!' => Some('！'),
            ';' => Some('；'),
            ':' => Some('：'),
            '.' if prev_cjk => Some('。'),
            _ => None,
        };
        match full {
            Some(f) if prev_cjk || (next_cjk && c != '.') => out.push(f),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn punct() {
        assert_eq!(fullwidth_punct("好,請說?"), "好，請說？");
        assert_eq!(fullwidth_punct("v1.2, OK"), "v1.2, OK");
        assert_eq!(fullwidth_punct("APP這邊.結束"), "APP這邊。結束");
        assert_eq!(fullwidth_punct("好啊..."), "好啊……");
        assert_eq!(fullwidth_punct("個人描述是。。。"), "個人描述是……");
        assert_eq!(fullwidth_punct("wait..."), "wait……");
    }
}

fn hms(t: f64) -> String {
    let t = t.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60)
}

fn srt_time(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

pub fn to_markdown(t: &Transcript, title: &str) -> String {
    let mut s = format!(
        "# {title} 逐字稿\n\n- 辨識方式：{}（{}）\n- 產生時間：{}\n- 音訊長度：{:.1} 分\n\n",
        t.engine,
        t.model,
        t.created_at,
        t.audio_seconds / 60.0
    );
    if !t.failed_chunks.is_empty() {
        s.push_str(&format!(
            "> 注意：以下時間開始的段落辨識失敗，需要重跑：{}\n\n",
            t.failed_chunks.iter().map(|c| hms(*c)).collect::<Vec<_>>().join("、")
        ));
    }
    for seg in &t.segments {
        let who = if seg.speaker.is_empty() { String::new() } else { format!("{}：", seg.speaker) };
        s.push_str(&format!("[{}] {}{}\n\n", hms(seg.start), who, seg.text));
    }
    s
}

pub fn to_srt(t: &Transcript) -> String {
    let mut s = String::new();
    for (i, seg) in t.segments.iter().enumerate() {
        let who = if seg.speaker.is_empty() { String::new() } else { format!("{}：", seg.speaker) };
        s.push_str(&format!("{}\n{} --> {}\n{}{}\n\n", i + 1, srt_time(seg.start), srt_time(seg.end), who, seg.text));
    }
    s
}

/// 寫出 transcript.<engine>.json、transcript.md、transcript.srt
pub fn write_outputs(dir: &Path, t: &Transcript, title: &str) -> Result<()> {
    std::fs::write(dir.join(format!("transcript.{}.json", t.engine)), serde_json::to_vec_pretty(t)?)?;
    std::fs::write(dir.join("transcript.md"), to_markdown(t, title))?;
    std::fs::write(dir.join("transcript.srt"), to_srt(t))?;
    Ok(())
}

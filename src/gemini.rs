//! Gemini 逐字稿與會議紀錄草稿（REST）。
//! 流程：16 kHz 音檔切段（預設 10 分、前後重疊 15 秒）→ Files API 上傳 → generateContent（JSON schema）
//! → 依段落邊界合併 → 刪除上傳檔。每一場都必須由使用者手動選擇才會上傳。

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::{Client, Response};
use serde_json::{Value, json};

use crate::audio::SAMPLE_RATE;
use crate::transcript::{PostProcess, Segment, Transcript, load_glossary};

const BASE: &str = "https://generativelanguage.googleapis.com";
const MAX_RETRY: u32 = 5;

pub struct Gemini {
    http: Client,
    key: String,
    pub model: String,
}

fn wav_bytes(samples: &[i16]) -> Vec<u8> {
    let data = (samples.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + data as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    v.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

/// "MM:SS"、"HH:MM:SS"、"MM:SS.mmm" 或純秒數 → 秒
pub fn parse_ts(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Ok(v) = s.parse::<f64>() {
        return Some(v);
    }
    let parts: Vec<&str> = s.split(':').collect();
    let nums: Vec<f64> = parts.iter().map(|p| p.trim().parse::<f64>()).collect::<Result<_, _>>().ok()?;
    match nums[..] {
        [m, s] => Some(m * 60.0 + s),
        [h, m, s] => Some(h * 3600.0 + m * 60.0 + s),
        _ => None,
    }
}

/// 一段的時間範圍：[start, end)，overlap 只用在送出的音訊，合併時以 [keep_from, keep_to) 為準
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chunk {
    pub start: f64,
    pub end: f64,
    pub keep_from: f64,
    pub keep_to: f64,
}

pub fn plan_chunks(total: f64, chunk: f64, overlap: f64) -> Vec<Chunk> {
    let mut v = Vec::new();
    let mut t = 0.0;
    while t < total {
        let keep_to = (t + chunk).min(total);
        v.push(Chunk { start: (t - overlap).max(0.0), end: (keep_to + overlap).min(total), keep_from: t, keep_to });
        t = keep_to;
    }
    v
}

/// 只留下起點落在本段負責範圍內的句子，重疊區的重複句因此只會保留一次
pub fn keep_segments(chunk: &Chunk, segs: Vec<Segment>) -> Vec<Segment> {
    segs.into_iter()
        .filter(|s| s.start >= chunk.keep_from - 0.5 && s.start < chunk.keep_to)
        .collect()
}

impl Gemini {
    pub fn new(model: &str) -> Result<Self> {
        let key = crate::keychain::gemini_key()?;
        let http = Client::builder().timeout(Duration::from_secs(600)).build()?;
        Ok(Self { http, key, model: model.to_string() })
    }

    /// 429／5xx／連線錯誤用指數退避重試，最多 5 次
    fn send(&self, build: impl Fn() -> reqwest::blocking::RequestBuilder) -> Result<Response> {
        let mut wait = Duration::from_secs(2);
        for attempt in 1..=MAX_RETRY {
            match build().header("x-goog-api-key", &self.key).send() {
                Ok(r) if r.status().is_success() => return Ok(r),
                Ok(r) if r.status().as_u16() == 429 || r.status().is_server_error() => {
                    if attempt == MAX_RETRY {
                        bail!("Gemini 回應 {}（重試 {MAX_RETRY} 次仍失敗）", r.status());
                    }
                }
                Ok(r) => {
                    let st = r.status();
                    let body = r.text().unwrap_or_default();
                    bail!("Gemini 回應 {st}：{}", body.chars().take(500).collect::<String>());
                }
                Err(e) if attempt == MAX_RETRY => return Err(e.into()),
                Err(_) => {}
            }
            std::thread::sleep(wait);
            wait *= 2;
        }
        unreachable!()
    }

    /// Files API resumable upload，等到 ACTIVE；回傳 (name, uri)
    fn upload(&self, bytes: Vec<u8>, mime: &str, display: &str) -> Result<(String, String)> {
        let len = bytes.len();
        let start = self.send(|| {
            self.http
                .post(format!("{BASE}/upload/v1beta/files"))
                .header("X-Goog-Upload-Protocol", "resumable")
                .header("X-Goog-Upload-Command", "start")
                .header("X-Goog-Upload-Header-Content-Length", len.to_string())
                .header("X-Goog-Upload-Header-Content-Type", mime)
                .json(&json!({ "file": { "display_name": display } }))
        })?;
        let url = start
            .headers()
            .get("x-goog-upload-url")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow!("上傳回應沒有 x-goog-upload-url"))?
            .to_string();
        let r: Value = self
            .send(|| {
                self.http
                    .post(&url)
                    .header("X-Goog-Upload-Offset", "0")
                    .header("X-Goog-Upload-Command", "upload, finalize")
                    .body(bytes.clone())
            })?
            .json()?;
        let name = r["file"]["name"].as_str().ok_or_else(|| anyhow!("上傳回應沒有 file.name"))?.to_string();
        let mut uri = r["file"]["uri"].as_str().unwrap_or_default().to_string();
        let mut state = r["file"]["state"].as_str().unwrap_or("ACTIVE").to_string();
        let t = Instant::now();
        while state == "PROCESSING" {
            if t.elapsed() > Duration::from_secs(300) {
                let _ = self.delete(&name);
                bail!("檔案處理逾時");
            }
            std::thread::sleep(Duration::from_secs(2));
            let f: Value = self.send(|| self.http.get(format!("{BASE}/v1beta/{name}")))?.json()?;
            state = f["state"].as_str().unwrap_or("ACTIVE").to_string();
            uri = f["uri"].as_str().unwrap_or(&uri).to_string();
        }
        if state == "FAILED" {
            let _ = self.delete(&name);
            bail!("Gemini 檔案處理失敗");
        }
        Ok((name, uri))
    }

    pub fn delete(&self, name: &str) -> Result<()> {
        self.send(|| self.http.delete(format!("{BASE}/v1beta/{name}")))?;
        Ok(())
    }

    fn generate(&self, parts: Value, schema: Option<Value>) -> Result<String> {
        let mut gen_cfg = json!({ "temperature": 0 });
        if let Some(s) = schema {
            gen_cfg["responseMimeType"] = json!("application/json");
            gen_cfg["responseSchema"] = s;
        }
        let body = json!({ "contents": [{ "role": "user", "parts": parts }], "generationConfig": gen_cfg });
        let r: Value = self
            .send(|| self.http.post(format!("{BASE}/v1beta/models/{}:generateContent", self.model)).json(&body))?
            .json()?;
        let cand = &r["candidates"][0];
        let finish = cand["finishReason"].as_str().unwrap_or("");
        let text: String = cand["content"]["parts"]
            .as_array()
            .map(|a| a.iter().filter_map(|p| p["text"].as_str()).collect())
            .unwrap_or_default();
        if text.is_empty() {
            bail!("Gemini 沒有回傳內容（finishReason={finish}）");
        }
        if finish == "MAX_TOKENS" {
            bail!("輸出被截斷（MAX_TOKENS），請把 chunk_minutes 調小");
        }
        Ok(text)
    }

    fn transcribe_chunk(&self, samples: &[i16], c: &Chunk, prev: Option<&Segment>, glossary: &[String]) -> Result<Vec<Segment>> {
        let a = (c.start * SAMPLE_RATE as f64) as usize;
        let b = ((c.end * SAMPLE_RATE as f64) as usize).min(samples.len());
        let (name, uri) = self.upload(wav_bytes(&samples[a..b]), "audio/wav", "meeting-chunk")?;
        let mut prompt = String::from(
            "請把這段會議錄音逐字轉寫成繁體中文（臺灣用語），英文技術名詞保留英文。\n\
             規則：\n\
             1. 照原話轉寫，不要潤飾、不要摘要、不要補字。\n\
             2. 聽不清楚的地方寫「[聽不清]」，不要猜。\n\
             3. 依講者分句，講者標成「講者A」「講者B」…；同一人在整段中使用同一標籤。\n\
             4. start、end 用這段音檔內的相對時間，格式 MM:SS。\n",
        );
        if !glossary.is_empty() {
            prompt.push_str(&format!("專有名詞：{}\n", glossary.join("、")));
        }
        if let Some(p) = prev {
            prompt.push_str(&format!(
                "前一段最後一句是 {}：「{}」。如果是同一個人接著講，請沿用相同的講者標籤。\n",
                p.speaker, p.text
            ));
        }
        let schema = json!({
            "type": "ARRAY",
            "items": {
                "type": "OBJECT",
                "properties": {
                    "start": { "type": "STRING" },
                    "end": { "type": "STRING" },
                    "speaker": { "type": "STRING" },
                    "text": { "type": "STRING" }
                },
                "required": ["start", "end", "speaker", "text"]
            }
        });
        let parts = json!([{ "file_data": { "mime_type": "audio/wav", "file_uri": uri } }, { "text": prompt }]);
        let result = self.generate(parts, Some(schema));
        // 不論成功與否都刪掉上傳檔，不等 48 小時自然過期
        let del = self.delete(&name);
        let text = result?;
        del.context("刪除 Gemini 上傳檔失敗")?;

        let arr: Vec<Value> = serde_json::from_str(&text).context("Gemini 回傳的不是預期的 JSON")?;
        let mut out = Vec::new();
        for v in arr {
            let (Some(s), Some(e)) = (v["start"].as_str().and_then(parse_ts), v["end"].as_str().and_then(parse_ts)) else {
                continue;
            };
            out.push(Segment {
                start: c.start + s,
                end: c.start + e,
                speaker: v["speaker"].as_str().unwrap_or("").to_string(),
                text: v["text"].as_str().unwrap_or("").trim().to_string(),
            });
        }
        Ok(out)
    }

    /// `only` 有值時只重跑這些段落（起始秒），其餘沿用 `previous`
    pub fn transcribe(
        &self,
        wav: &Path,
        chunk_minutes: u32,
        overlap_seconds: u32,
        previous: Option<&Transcript>,
        mut progress: impl FnMut(usize, usize),
    ) -> Result<Transcript> {
        let samples: Vec<i16> = crate::audio::read_wav_f32(wav)?.iter().map(|f| (f * 32768.0) as i16).collect();
        let total = samples.len() as f64 / SAMPLE_RATE as f64;
        let chunks = plan_chunks(total, chunk_minutes as f64 * 60.0, overlap_seconds as f64);
        let glossary = load_glossary();
        let pp = PostProcess::new()?;
        let started = Instant::now();
        let mut segs: Vec<Segment> = Vec::new();
        let mut failed = Vec::new();
        for (i, c) in chunks.iter().enumerate() {
            progress(i, chunks.len());
            if let Some(p) = previous {
                if !p.failed_chunks.iter().any(|f| (f - c.keep_from).abs() < 1.0) {
                    segs.extend(p.segments.iter().filter(|s| s.start >= c.keep_from && s.start < c.keep_to).cloned());
                    continue;
                }
            }
            match self.transcribe_chunk(&samples, c, segs.last(), &glossary) {
                Ok(v) => segs.extend(keep_segments(c, v).into_iter().map(|mut s| {
                    s.text = pp.apply(&s.text);
                    s
                })),
                Err(e) => {
                    eprintln!("第 {} 段（{:.0} 秒起）失敗：{e:#}", i + 1, c.keep_from);
                    failed.push(c.keep_from);
                }
            }
        }
        progress(chunks.len(), chunks.len());
        segs.sort_by(|a, b| a.start.total_cmp(&b.start));
        Ok(Transcript {
            engine: "gemini".into(),
            model: self.model.clone(),
            created_at: chrono::Local::now().to_rfc3339(),
            audio_seconds: total,
            elapsed_seconds: started.elapsed().as_secs_f64(),
            segments: segs,
            failed_chunks: failed,
        })
    }

    /// 會議紀錄草稿：輸入整理好的逐字稿 md，每條決議與待辦都要附時間碼
    pub fn minutes(&self, transcript_md: &str) -> Result<String> {
        let prompt = format!(
            "以下是一場會議的逐字稿（每行開頭是 [時:分:秒] 時間碼）。請用繁體中文（臺灣）產生會議紀錄草稿，Markdown 格式，章節依序為：\n\
             ## 會議基本資料（日期、時間、地點、出席；逐字稿沒有的留「（待補）」）\n\
             ## 討論事項（分層編號 1.、1.1）\n\
             ## 決議\n\
             ## 待辦事項（表格：項目｜負責人｜期限｜時間碼）\n\
             ## 下次會議前置作業\n\n\
             硬性規則：\n\
             - 每一條討論重點、決議、待辦都要在句尾附上依據的逐字稿時間碼，格式〔hh:mm:ss〕。\n\
             - 找不到時間碼依據的內容一律不要寫。\n\
             - 不要推測負責人或期限；逐字稿沒講就寫「（未提及）」。\n\
             - 不要加入逐字稿以外的資訊。\n\n\
             逐字稿：\n{transcript_md}"
        );
        self.generate(json!([{ "text": prompt }]), None)
    }
}

/// 檢查草稿：決議、待辦段落裡每個條目都要有時間碼，回傳缺少時間碼的行
pub fn lines_missing_timecode(md: &str) -> Vec<String> {
    let mut in_section = false;
    let mut out = Vec::new();
    for line in md.lines() {
        if line.starts_with("## ") {
            in_section = line.contains("決議") || line.contains("待辦") || line.contains("討論");
            continue;
        }
        let l = line.trim();
        let is_item = l.starts_with('-') || l.starts_with('*') || l.chars().next().is_some_and(|c| c.is_ascii_digit())
            || (l.starts_with('|') && !l.contains("---") && !l.contains("項目"));
        if in_section && is_item && !l.contains('〔') && !l.contains("（未提及）") {
            out.push(l.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts() {
        assert_eq!(parse_ts("01:30"), Some(90.0));
        assert_eq!(parse_ts("1:02:03"), Some(3723.0));
        assert_eq!(parse_ts("12.5"), Some(12.5));
        assert_eq!(parse_ts("ab"), None);
    }

    #[test]
    fn chunks_cover_everything_once() {
        let c = plan_chunks(1500.0, 600.0, 15.0);
        assert_eq!(c.len(), 3);
        assert_eq!(c[0], Chunk { start: 0.0, end: 615.0, keep_from: 0.0, keep_to: 600.0 });
        assert_eq!(c[1], Chunk { start: 585.0, end: 1215.0, keep_from: 600.0, keep_to: 1200.0 });
        assert_eq!(c[2], Chunk { start: 1185.0, end: 1500.0, keep_from: 1200.0, keep_to: 1500.0 });
    }

    #[test]
    fn overlap_dedup() {
        let c = Chunk { start: 585.0, end: 1215.0, keep_from: 600.0, keep_to: 1200.0 };
        let mk = |s: f64| Segment { start: s, end: s + 2.0, speaker: "A".into(), text: format!("{s}") };
        let kept = keep_segments(&c, vec![mk(586.0), mk(600.2), mk(900.0), mk(1199.0), mk(1201.0)]);
        assert_eq!(kept.iter().map(|s| s.start).collect::<Vec<_>>(), vec![600.2, 900.0, 1199.0]);
    }

    #[test]
    fn timecode_check() {
        let md = "## 決議\n- 同意上線〔00:12:03〕\n- 沒有依據的決議\n## 其他\n- 隨便";
        assert_eq!(lines_missing_timecode(md), vec!["- 沒有依據的決議".to_string()]);
    }
}

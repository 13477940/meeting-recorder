mod app;
mod audio;
mod config;
mod disk;
mod gemini;
mod jobs;
mod keychain;
mod models;
mod local_asr;
mod recorder;
mod session;
mod transcript;
mod writer;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use recorder::{RecordOptions, Recording, Target};
use writer::Codec;

const USAGE: &str = "\
meeting-recorder — 會議錄影、錄音與逐字稿

用法：
  meeting-recorder                         不帶參數：啟動選單列 App
  meeting-recorder sources
  meeting-recorder record [選項]
      --name <會議名稱>        預設用時間
      --root <輸出根目錄>      預設 ~/Movies/MeetingRecorder
      --display <id> | --app <名稱> | --region x,y,w,h   預設主螢幕
      --fps <1|2|5|15>         預設 2
      --res <720|1080>         預設 720
      --codec <hevc|h264>      預設 hevc
      --bitrate <kbps>         預設 400
      --fragment <秒>          預設 10
      --no-system-audio  --no-mic  --no-cursor
      --mic-device <id>
      --duration <秒>          到時自動停止（測試用）
      --pause-at <秒> --resume-at <秒>   測試暫停（以錄影開始起算）
  錄影中按 Ctrl-C 停止並存檔。
  meeting-recorder transcribe <場次資料夾|錄影檔> [--model large-v3-turbo|medium] [--no-vad] [--out <資料夾>]
      本機辨識（whisper.cpp + Metal），輸出 transcript.local.json／transcript.md／transcript.srt
      --engine gemini --confirm-upload   改用 Gemini（會把 16 kHz 音檔上傳到 Google；每場都要手動加這個旗標）
      --retry-failed                     只重跑上次 Gemini 失敗的段落
  meeting-recorder minutes <場次資料夾> --confirm-upload   用 Gemini 由 transcript.md 產生 minutes_draft.md
  meeting-recorder models list | models download <名稱>
  meeting-recorder gemini set-key | gemini delete-key
  meeting-recorder config                  建立／顯示設定檔位置
  meeting-recorder extract-audio <錄影檔> <輸出.wav>   取出 16 kHz 單聲道音檔
";

fn main() {
    if let Err(e) = run() {
        eprintln!("錯誤：{e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // 沒有參數（從 Finder／open 啟動 .app）就進入選單列 App
    let Some(cmd) = args.first().filter(|a| !a.starts_with("-psn_")) else {
        app::run();
        return Ok(());
    };
    let rest = &args[1..];
    match cmd.as_str() {
        "sources" => {
            print!("{}", recorder::list_sources()?);
            Ok(())
        }
        "record" => cmd_record(rest),
        "transcribe" => cmd_transcribe(rest),
        "minutes" => cmd_minutes(rest),
        "models" => match rest {
            [sub, name] if sub == "download" => models::download(name),
            [sub] if sub == "list" => {
                print!("{}", models::list());
                Ok(())
            }
            _ => bail!("用法：models list | models download <large-v3-turbo|medium|silero-vad>"),
        },
        "gemini" => match rest.first().map(String::as_str) {
            Some("set-key") => {
                eprintln!("貼上 Gemini API key 後按 Enter（只會存進 macOS Keychain）：");
                let mut k = String::new();
                std::io::stdin().read_line(&mut k)?;
                if k.trim().is_empty() {
                    bail!("沒有輸入 key");
                }
                keychain::set_gemini_key(&k)?;
                eprintln!("已存入 Keychain（service=MeetingRecorder）");
                Ok(())
            }
            Some("delete-key") => keychain::delete_gemini_key(),
            _ => bail!("用法：gemini set-key | gemini delete-key"),
        },
        "config" => {
            config::Config::load()?;
            println!("{}", config::path().display());
            Ok(())
        }
        "extract-audio" => {
            let [input, output] = rest else { bail!("用法：extract-audio <錄影檔> <輸出.wav>") };
            let t = std::time::Instant::now();
            let secs = audio::extract_16k(input.as_ref(), output.as_ref())?;
            eprintln!("完成：{output}（音訊 {:.1} 分，耗時 {:.1} 秒）", secs / 60.0, t.elapsed().as_secs_f64());
            Ok(())
        }
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        other => bail!("不認得的指令：{other}\n\n{USAGE}"),
    }
}

struct Args<'a> {
    v: &'a [String],
    i: usize,
}
impl<'a> Args<'a> {
    fn value(&mut self, flag: &str) -> Result<&'a str> {
        self.i += 1;
        self.v.get(self.i).map(|s| s.as_str()).ok_or_else(|| anyhow!("{flag} 後面要接值"))
    }
}

fn cmd_record(rest: &[String]) -> Result<()> {
    let mut o = record_options_from_config(&config::Config::load()?);
    let mut duration: Option<f64> = None;
    let mut pause_at: Option<f64> = None;
    let mut resume_at: Option<f64> = None;
    let mut a = Args { v: rest, i: 0 };
    while a.i < rest.len() {
        let f = rest[a.i].as_str();
        match f {
            "--name" => o.name = a.value(f)?.into(),
            "--root" => o.root = PathBuf::from(a.value(f)?),
            "--display" => o.target = Target::Display { id: Some(a.value(f)?.parse()?) },
            "--app" => o.target = Target::App { name: a.value(f)?.into() },
            "--region" => {
                let p: Vec<f64> = a.value(f)?.split(',').map(|s| s.trim().parse()).collect::<Result<_, _>>()?;
                let [x, y, w, h] = p[..] else { bail!("--region 格式為 x,y,w,h") };
                o.target = Target::Region { x, y, w, h };
            }
            "--fps" => o.fps = a.value(f)?.parse()?,
            "--res" => o.resolution = a.value(f)?.parse()?,
            "--codec" => {
                o.codec = match a.value(f)? {
                    "hevc" => Codec::Hevc,
                    "h264" => Codec::H264,
                    v => bail!("不支援的編碼 {v}"),
                }
            }
            "--bitrate" => o.video_bitrate_kbps = a.value(f)?.parse()?,
            "--fragment" => o.fragment_seconds = a.value(f)?.parse()?,
            "--no-system-audio" => o.system_audio = false,
            "--no-mic" => o.mic = false,
            "--no-cursor" => o.show_cursor = false,
            "--mic-device" => o.mic_device = Some(a.value(f)?.into()),
            "--duration" => duration = Some(a.value(f)?.parse()?),
            "--pause-at" => pause_at = Some(a.value(f)?.parse()?),
            "--resume-at" => resume_at = Some(a.value(f)?.parse()?),
            _ => bail!("不認得的選項：{f}"),
        }
        a.i += 1;
    }
    if !matches!(o.fps, 1 | 2 | 5 | 15 | 30) {
        bail!("--fps 只接受 1、2、5、15（30 供比對用）");
    }

    for d in session::find_unfinished(&o.root) {
        eprintln!("注意：上一場沒有正常結束，請檢查 {}", d.display());
    }

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst))?;
    }

    let mut rec = Recording::start(&o)?;
    eprintln!("錄影中 → {}（Ctrl-C 停止）", rec.dir.display());
    let wall = std::time::Instant::now();
    let mut reason = None;
    let mut paused_done = false;
    let mut resumed_done = false;
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let t = wall.elapsed().as_secs_f64();
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if let Some(e) = rec.error() {
            eprintln!("錄影錯誤：{e}");
            break;
        }
        if !paused_done && pause_at.is_some_and(|p| t >= p) {
            rec.pause()?;
            paused_done = true;
            eprintln!("[{t:.1}s] 暫停");
        }
        if paused_done && !resumed_done && resume_at.is_some_and(|r| t >= r) {
            rec.resume()?;
            resumed_done = true;
            eprintln!("[{t:.1}s] 繼續");
        }
        if duration.is_some_and(|d| t >= d) {
            break;
        }
        if rec.disk_low() {
            reason = Some("磁碟剩餘不到 500 MB，自動停止".to_string());
            eprintln!("磁碟剩餘不到 500 MB，自動停止並存檔");
            break;
        }
    }
    let elapsed = rec.elapsed();
    let dir = rec.stop(reason)?;
    let s = session::Session::load(&dir)?;
    eprintln!(
        "完成：{}（{:.1} 秒，畫面 {} 張／系統聲音 {} 段／麥克風 {} 段）",
        dir.display(),
        elapsed.as_secs_f64(),
        s.frames.map(|f| f[0]).unwrap_or(0),
        s.frames.map(|f| f[1]).unwrap_or(0),
        s.frames.map(|f| f[2]).unwrap_or(0)
    );
    Ok(())
}

fn cmd_transcribe(rest: &[String]) -> Result<()> {
    let mut cfg = config::Config::load()?;
    let mut input = None;
    let mut out = None;
    let mut engine = jobs::Engine::Local;
    let mut confirm = false;
    let mut retry_failed = false;
    let mut a = Args { v: rest, i: 0 };
    while a.i < rest.len() {
        let f = rest[a.i].as_str();
        match f {
            "--model" => cfg.transcribe.local.model = a.value(f)?.into(),
            "--out" => out = Some(PathBuf::from(a.value(f)?)),
            "--no-vad" => cfg.transcribe.local.vad = false,
            "--engine" => {
                engine = match a.value(f)? {
                    "local" => jobs::Engine::Local,
                    "gemini" => jobs::Engine::Gemini,
                    e => bail!("--engine 只接受 local 或 gemini，收到 {e}"),
                }
            }
            "--confirm-upload" => confirm = true,
            "--retry-failed" => retry_failed = true,
            _ if !f.starts_with("--") && input.is_none() => input = Some(PathBuf::from(f)),
            _ => bail!("不認得的選項：{f}"),
        }
        a.i += 1;
    }
    let input = input.ok_or_else(|| anyhow!("要指定場次資料夾或錄影檔"))?;
    if engine == jobs::Engine::Gemini && !confirm {
        bail!("Gemini 會把這場的音檔上傳到 Google。確定要上傳，請加上 --confirm-upload");
    }
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let c = cancel.clone();
        ctrlc::set_handler(move || c.store(true, Ordering::SeqCst))?;
    }
    let mut last = String::new();
    let (md, t) = jobs::transcribe(&input, out, engine, retry_failed, &cfg, cancel, move |msg| {
        if msg != last {
            eprint!("\r{msg}        ");
            last = msg;
        }
    })?;
    eprintln!();
    if !t.failed_chunks.is_empty() {
        eprintln!("有 {} 段失敗，可用 --retry-failed 重跑", t.failed_chunks.len());
    }
    eprintln!(
        "完成：{}（音訊 {:.1} 分，耗時 {:.1} 分，{} 段）",
        md.display(),
        t.audio_seconds / 60.0,
        t.elapsed_seconds / 60.0,
        t.segments.len()
    );
    Ok(())
}

fn cmd_minutes(rest: &[String]) -> Result<()> {
    let [dir, flag] = rest else { bail!("用法：minutes <場次資料夾> --confirm-upload") };
    if flag != "--confirm-upload" {
        bail!("產生草稿會把逐字稿送到 Google。確定要送，請加上 --confirm-upload");
    }
    let cfg = config::Config::load()?;
    eprintln!("產生會議紀錄草稿中（{}）", cfg.transcribe.gemini.model);
    let (p, missing) = jobs::minutes(std::path::Path::new(dir), &cfg)?;
    if missing > 0 {
        eprintln!("注意：{missing} 條沒有時間碼，已列在草稿最後");
    }
    eprintln!("完成：{}", p.display());
    Ok(())
}

pub(crate) fn record_options_from_config(c: &config::Config) -> RecordOptions {
    let target = match c.capture.target.as_str() {
        "window" if !c.capture.app.is_empty() => Target::App { name: c.capture.app.clone() },
        "region" => {
            let [x, y, w, h] = c.capture.region;
            Target::Region { x, y, w, h }
        }
        _ => Target::Display { id: None },
    };
    RecordOptions {
        name: String::new(),
        root: c.output_root(),
        target,
        fps: c.capture.fps,
        resolution: c.capture.resolution,
        codec: c.encode.video_codec,
        video_bitrate_kbps: c.encode.video_bitrate_kbps,
        fragment_seconds: c.encode.fragment_seconds,
        show_cursor: c.capture.show_cursor,
        system_audio: c.capture.capture_system_audio,
        mic: c.capture.capture_mic,
        mic_device: (c.capture.mic_device != "default" && !c.capture.mic_device.is_empty()).then(|| c.capture.mic_device.clone()),
    }
}

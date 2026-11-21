//! 擷取層：ScreenCaptureKit 的 SCStream 把畫面、系統聲音、麥克風交給 Writer。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use screencapturekit::cm::SCFrameStatus;
use screencapturekit::prelude::*;

use crate::common::{RecordOptions, Target, output_size};
use crate::session::{Session, Status};
use super::writer::{Track, Writer, WriterConfig};

/// SCStream 回呼和控制端共用的狀態
struct Shared {
    writer: Mutex<Option<Writer>>,
    error: Mutex<Option<String>>,
    failed: AtomicBool,
}

struct Output {
    shared: Arc<Shared>,
}

impl SCStreamOutputTrait for Output {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        let track = match of_type {
            SCStreamOutputType::Screen => {
                // 畫面沒變時 SCK 會送 Idle 之類沒有影像的 buffer，跳過
                if sample.frame_status() != Some(SCFrameStatus::Complete) {
                    return;
                }
                Track::Video
            }
            SCStreamOutputType::Audio => Track::System,
            SCStreamOutputType::Microphone => Track::Mic,
        };
        if self.shared.failed.load(Ordering::Relaxed) {
            return;
        }
        let mut guard = self.shared.writer.lock().unwrap();
        let Some(w) = guard.as_mut() else { return };
        if let Err(e) = w.append(track, sample.as_ptr()) {
            self.shared.failed.store(true, Ordering::Relaxed);
            *self.shared.error.lock().unwrap() = Some(e.to_string());
        }
    }
}

pub struct Recording {
    pub dir: PathBuf,
    stream: SCStream,
    shared: Arc<Shared>,
    session: Session,
    started: Instant,
    paused_since: Option<Instant>,
    paused_total: Duration,
}

fn host_now() -> f64 {
    let clock = unsafe { objc2_core_media::CMClock::host_time_clock() };
    super::writer::cm_seconds(unsafe { clock.time() })
}

impl Recording {
    pub fn start(opts: &RecordOptions) -> Result<Self> {
        std::fs::create_dir_all(&opts.root).with_context(|| format!("無法建立 {}", opts.root.display()))?;
        if let Some(free) = crate::disk::free_bytes(&opts.root) {
            if free < crate::common::MIN_FREE_TO_START {
                bail!("磁碟剩餘 {:.1} GB，不到 2 GB，請先清出空間", free as f64 / 1e9);
            }
        }

        let content = SCShareableContent::get().map_err(|e| {
            anyhow!("取得螢幕內容失敗（多半是沒有「螢幕與系統錄音」權限）：{e:?}")
        })?;
        let displays = content.displays();
        let main = displays.first().cloned().ok_or_else(|| anyhow!("找不到螢幕"))?;

        let (filter, src_w, src_h, source_rect) = match &opts.target {
            Target::Display { id } => {
                let d = match id {
                    Some(id) => displays
                        .iter()
                        .find(|d| d.display_id() == *id)
                        .cloned()
                        .ok_or_else(|| anyhow!("找不到 display {id}"))?,
                    None => main.clone(),
                };
                let f = SCContentFilter::create().with_display(&d).with_excluding_windows(&[]).build()?;
                (f, d.width() as f64, d.height() as f64, None)
            }
            Target::App { name } => {
                let needle = name.to_lowercase();
                let win = content
                    .windows()
                    .into_iter()
                    .filter(|w| w.is_on_screen() && w.frame().size.width > 200.0 && w.frame().size.height > 150.0)
                    .find(|w| {
                        w.owning_application().is_some_and(|a| {
                            a.application_name().to_lowercase().contains(&needle)
                                || a.bundle_identifier().to_lowercase().contains(&needle)
                        })
                    })
                    .ok_or_else(|| anyhow!("找不到「{name}」在畫面上的視窗"))?;
                let fr = win.frame();
                let f = SCContentFilter::create().with_window(&win).build()?;
                (f, fr.size.width, fr.size.height, None)
            }
            Target::Region { x, y, w, h } => {
                let f = SCContentFilter::create().with_display(&main).with_excluding_windows(&[]).build()?;
                (f, *w, *h, Some(CGRect::new(*x, *y, *w, *h)))
            }
        };
        // SCDisplay 寬高是點；Retina 實際像素是兩倍，輸出尺寸以點計算即可
        let (width, height) = output_size(src_w, src_h, opts.resolution);

        let mut config = SCStreamConfiguration::new()
            .with_width(width)
            .with_height(height)
            .with_fps(opts.fps)
            .with_pixel_format(PixelFormat::YCbCr_420v)
            .with_shows_cursor(opts.show_cursor)
            .with_queue_depth(5)
            .with_captures_audio(opts.system_audio)
            .with_sample_rate(48_000)
            .with_channel_count(2)
            .with_excludes_current_process_audio(true);
        if let Some(r) = source_rect {
            config = config.with_source_rect(r);
        }
        if opts.mic {
            config = config.with_captures_microphone(true)?;
            if let Some(dev) = &opts.mic_device {
                config = config.with_microphone_capture_device_id(dev)?;
            }
        }

        let (dir, now) = crate::common::new_session_dir(opts)?;

        let wcfg = WriterConfig {
            width,
            height,
            fps: opts.fps,
            codec: opts.codec,
            video_bitrate_kbps: opts.video_bitrate_kbps,
            fragment_seconds: opts.fragment_seconds,
            system_audio: opts.system_audio,
            mic: opts.mic,
            quicktime: true,
        };
        let file_name = "recording.mov";
        let writer = Writer::new(&dir.join(file_name), &wcfg)?;

        let session = crate::common::new_session(opts, now, file_name, width, height);
        session.save(&dir)?;

        let shared = Arc::new(Shared { writer: Mutex::new(Some(writer)), error: Mutex::new(None), failed: AtomicBool::new(false) });
        let mut stream = SCStream::new(&filter, &config)?;
        stream.add_output_handler(Output { shared: shared.clone() }, SCStreamOutputType::Screen)?;
        if opts.system_audio {
            stream.add_output_handler(Output { shared: shared.clone() }, SCStreamOutputType::Audio)?;
        }
        if opts.mic {
            stream.add_output_handler(Output { shared: shared.clone() }, SCStreamOutputType::Microphone)?;
        }
        if let Err(e) = stream.start_capture() {
            let mut s = session.clone();
            s.status = Status::Failed;
            s.error = Some(format!("{e:?}"));
            let _ = s.save(&dir);
            bail!("開始擷取失敗：{e:?}");
        }

        Ok(Self { dir, stream, shared, session, started: Instant::now(), paused_since: None, paused_total: Duration::ZERO })
    }

    pub fn is_paused(&self) -> bool {
        self.paused_since.is_some()
    }

    /// 已錄的長度（不含暫停）
    pub fn elapsed(&self) -> Duration {
        let paused = self.paused_total + self.paused_since.map(|p| p.elapsed()).unwrap_or_default();
        self.started.elapsed().saturating_sub(paused)
    }

    pub fn pause(&mut self) -> Result<()> {
        if self.paused_since.is_some() {
            return Ok(());
        }
        if let Some(w) = self.shared.writer.lock().unwrap().as_mut() {
            w.pause(host_now());
        }
        self.paused_since = Some(Instant::now());
        self.session.status = Status::Paused;
        self.session.save(&self.dir)
    }

    pub fn resume(&mut self) -> Result<()> {
        let Some(p) = self.paused_since.take() else { return Ok(()) };
        self.paused_total += p.elapsed();
        if let Some(w) = self.shared.writer.lock().unwrap().as_mut() {
            w.resume(host_now());
            self.session.paused_seconds = w.paused_seconds();
        }
        self.session.status = Status::Recording;
        self.session.save(&self.dir)
    }

    /// 錄影途中發生的錯誤（例如寫檔失敗）
    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    /// 剩餘空間是否已低於下限
    pub fn disk_low(&self) -> bool {
        crate::disk::free_bytes(&self.dir).is_some_and(|f| f < crate::common::MIN_FREE_WHILE_RECORDING)
    }

    pub fn stop(mut self, reason: Option<String>) -> Result<PathBuf> {
        let _ = self.stream.stop_capture();
        if self.paused_since.is_some() {
            let _ = self.resume();
        }
        let writer = self.shared.writer.lock().unwrap().take();
        let mut result = match writer {
            Some(w) => {
                self.session.frames = Some(w.counts);
                self.session.paused_seconds = w.paused_seconds();
                w.finish()
            }
            None => Err(anyhow!("writer 已不存在")),
        };
        if let Some(e) = self.error() {
            result = Err(anyhow!(e));
        }
        self.session.ended_at = Some(chrono::Local::now().to_rfc3339());
        self.session.duration_seconds = Some(self.elapsed().as_secs_f64());
        match &result {
            Ok(()) => {
                self.session.status = Status::Finished;
                self.session.error = reason;
            }
            Err(e) => {
                self.session.status = Status::Failed;
                self.session.error = Some(e.to_string());
            }
        }
        self.session.save(&self.dir)?;
        result.map(|_| self.dir.clone())
    }
}

/// 列出可選的螢幕、視窗、麥克風
pub fn list_sources() -> Result<String> {
    let mut out = String::new();
    let content = SCShareableContent::get().map_err(|e| anyhow!("取得螢幕內容失敗：{e:?}"))?;
    out.push_str("螢幕：\n");
    for d in content.displays() {
        out.push_str(&format!("  id={} {}x{}\n", d.display_id(), d.width(), d.height()));
    }
    out.push_str("視窗（在畫面上）：\n");
    for w in content.windows().into_iter().filter(|w| w.is_on_screen()) {
        let app = w.owning_application().map(|a| a.application_name()).unwrap_or_default();
        let fr = w.frame();
        if fr.size.width < 200.0 {
            continue;
        }
        out.push_str(&format!("  {} — {} ({:.0}x{:.0})\n", app, w.title().unwrap_or_default(), fr.size.width, fr.size.height));
    }
    out.push_str("麥克風：\n");
    for m in AudioInputDevice::list() {
        out.push_str(&format!("  {}{} id={}\n", if m.is_default { "* " } else { "  " }, m.name, m.id));
    }
    Ok(out)
}


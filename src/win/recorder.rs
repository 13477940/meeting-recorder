//! Windows 擷取層：Windows.Graphics.Capture（windows-capture）取畫面，WASAPI 取系統聲音（loopback）與麥克風，
//! 全部交給 Media Foundation SinkWriter 寫成 fragmented MP4。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};
use windows_capture::capture::{CaptureControl, Context as CaptureContext, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings, MinimumUpdateIntervalSettings,
    SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use super::sink::{AUDIO_RATE, MIC_CHANNELS, SYSTEM_CHANNELS, Sink, SinkConfig, Track, scale_bgra};
use crate::common::{RecordOptions, Target, output_size};
use crate::session::{Session, Status};

/// 畫面靜止時每隔多久補寫一次最後一張，讓 fragment 持續前進、播放器拖曳順暢
const KEEPALIVE: Duration = Duration::from_secs(5);

/// 錄影時鐘：100 ns 單位，扣掉暫停的時間
struct Clock {
    start: Instant,
    paused_total: Duration,
    paused_since: Option<Instant>,
}

impl Clock {
    fn now_hns(&self) -> i64 {
        let paused = self.paused_total + self.paused_since.map(|p| p.elapsed()).unwrap_or_default();
        (self.start.elapsed().saturating_sub(paused).as_nanos() / 100) as i64
    }
}

struct Shared {
    sink: Mutex<Option<Sink>>,
    clock: Mutex<Clock>,
    paused: AtomicBool,
    stop: AtomicBool,
    failed: AtomicBool,
    error: Mutex<Option<String>>,
    /// 最後寫入的畫面與時間，給 keepalive 用
    last_frame: Mutex<Option<(Vec<u8>, Instant)>>,
}

impl Shared {
    fn fail(&self, e: impl std::fmt::Display) {
        self.failed.store(true, Ordering::SeqCst);
        *self.error.lock().unwrap() = Some(e.to_string());
    }

    fn write_video(&self, bgra: &[u8]) {
        if self.paused.load(Ordering::Relaxed) || self.failed.load(Ordering::Relaxed) {
            return;
        }
        let ts = self.clock.lock().unwrap().now_hns();
        if let Some(s) = self.sink.lock().unwrap().as_mut() {
            if let Err(e) = s.write_video(bgra, ts) {
                self.fail(format!("{e:#}"));
            }
        }
    }
}

struct Flags {
    shared: Arc<Shared>,
    out_w: u32,
    out_h: u32,
    fps: u32,
    /// 區域模式的裁切範圍（像素）；None = 整張
    crop: Option<(u32, u32, u32, u32)>,
}

struct Capturer {
    f: Flags,
    last: Option<Instant>,
    raw: Vec<u8>,
    scaled: Vec<u8>,
}

impl GraphicsCaptureApiHandler for Capturer {
    type Flags = Flags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: CaptureContext<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { f: ctx.flags, last: None, raw: Vec::new(), scaled: Vec::new() })
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame, control: InternalCaptureControl) -> Result<(), Self::Error> {
        if self.f.shared.stop.load(Ordering::Relaxed) {
            control.stop();
            return Ok(());
        }
        // Windows 只在畫面有變化時送畫面；這裡再依設定的每秒張數節流
        let min_gap = Duration::from_millis(1000 / self.f.fps.max(1) as u64);
        if self.last.is_some_and(|t| t.elapsed() < min_gap) {
            return Ok(());
        }
        self.last = Some(Instant::now());
        let (w, h) = (frame.width(), frame.height());
        let buf = frame.buffer()?;
        let data = buf.as_nopadding_buffer(&mut self.raw).to_vec();
        let crop = self.f.crop.map(|(x, y, cw, ch)| (x.min(w - 1), y.min(h - 1), cw.min(w - x.min(w - 1)), ch.min(h - y.min(h - 1)))).unwrap_or((0, 0, w, h));
        scale_bgra(&data, w, h, crop, self.f.out_w, self.f.out_h, &mut self.scaled);
        self.f.shared.write_video(&self.scaled);
        *self.f.shared.last_frame.lock().unwrap() = Some((self.scaled.clone(), Instant::now()));
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.f.shared.fail("擷取的視窗已關閉，錄影已停止");
        Ok(())
    }
}

/// 一條 WASAPI 擷取執行緒；Track::System 是系統聲音（loopback）
fn audio_thread(shared: Arc<Shared>, track: Track, device_id: Option<String>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        if let Err(e) = audio_loop(&shared, track, device_id) {
            shared.fail(format!("{:?} 擷取失敗：{e:#}", track));
        }
    })
}

fn audio_loop(shared: &Shared, track: Track, device_id: Option<String>) -> Result<()> {
    let _ = wasapi::initialize_mta();
    let en = DeviceEnumerator::new().map_err(|e| anyhow!("{e}"))?;
    let (device, channels) = match track {
        Track::System => (en.get_default_device(Direction::Render).map_err(|e| anyhow!("{e}"))?, SYSTEM_CHANNELS),
        _ => {
            let d = match device_id {
                Some(id) => {
                    let col = en.get_device_collection(Direction::Capture).map_err(|e| anyhow!("{e}"))?;
                    let mut found = None;
                    for d in &col {
                        let d = d.map_err(|e| anyhow!("{e}"))?;
                        if d.get_id().ok().as_deref() == Some(id.as_str()) {
                            found = Some(d);
                            break;
                        }
                    }
                    found.ok_or_else(|| anyhow!("找不到麥克風 {id}"))?
                }
                None => en.get_default_device(Direction::Capture).map_err(|e| anyhow!("{e}"))?,
            };
            (d, MIC_CHANNELS)
        }
    };
    let mut client = device.get_iaudioclient().map_err(|e| anyhow!("{e}"))?;
    let fmt = WaveFormat::new(16, 16, SampleType::Int, AUDIO_RATE as usize, channels as usize, None);
    let (_def, min) = client.get_device_period().map_err(|e| anyhow!("{e}"))?;
    // 共用模式＋autoconvert：由 Windows 轉成 48 kHz 16-bit；render 裝置配 Capture 方向即 loopback
    let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: min };
    client.initialize_client(&fmt, Direction::Capture, mode, None).map_err(|e| anyhow!("{e}"))?;
    let ev = client.set_get_eventhandle().map_err(|e| anyhow!("{e}"))?;
    let cap = client.get_audiocaptureclient().map_err(|e| anyhow!("{e}"))?;
    client.start_stream().map_err(|e| anyhow!("{e}"))?;

    let block = (channels * 2) as usize;
    let mut q = std::collections::VecDeque::<u8>::new();
    // 以「段落起點＋已寫樣本數」算時間戳，避免封包到達時間的抖動
    let mut seg_base: Option<i64> = None;
    let mut seg_frames: i64 = 0;
    let chunk = (AUDIO_RATE as usize / 10) * block; // 每 100 ms 寫一次
    while !shared.stop.load(Ordering::Relaxed) {
        // loopback 在沒有聲音播放時不會觸發事件；逾時照樣往下讀，並視需要補靜音
        let _ = ev.wait_for_event(200);
        cap.read_from_device_to_deque(&mut q).map_err(|e| anyhow!("{e}"))?;
        if shared.paused.load(Ordering::Relaxed) {
            q.clear();
            seg_base = None;
            continue;
        }
        // loopback 沒聲音時補靜音，音軌才會和畫面一樣長
        if track == Track::System && seg_base.is_none() {
            seg_base = Some(shared.clock.lock().unwrap().now_hns());
            seg_frames = 0;
        }
        if track == Track::System && q.is_empty() {
            if let Some(base) = seg_base {
                let now = shared.clock.lock().unwrap().now_hns();
                let written_until = base + seg_frames * 10_000_000 / AUDIO_RATE as i64;
                let gap = now - written_until;
                if gap > 2_000_000 {
                    let frames = (gap * AUDIO_RATE as i64 / 10_000_000) as usize;
                    q.extend(std::iter::repeat_n(0u8, frames * block));
                }
            }
        }
        if q.len() >= chunk {
            let n = q.len() / block * block;
            let data: Vec<u8> = q.drain(..n).collect();
            let base = match seg_base {
                Some(b) => b,
                None => {
                    let b = shared.clock.lock().unwrap().now_hns();
                    seg_base = Some(b);
                    seg_frames = 0;
                    b
                }
            };
            let ts = base + seg_frames * 10_000_000 / AUDIO_RATE as i64;
            seg_frames += (n / block) as i64;
            if let Some(s) = shared.sink.lock().unwrap().as_mut() {
                s.write_audio(track, &data, ts).context("寫入音訊失敗")?;
            }
        }
    }
    let _ = client.stop_stream();
    Ok(())
}

pub struct Recording {
    pub dir: PathBuf,
    shared: Arc<Shared>,
    capture: Option<CaptureControl<Capturer, Box<dyn std::error::Error + Send + Sync>>>,
    threads: Vec<JoinHandle<()>>,
    session: Session,
}

fn monitor_for(target: &Target) -> Result<Monitor> {
    match target {
        Target::Display { id: Some(i) } => Monitor::enumerate()?
            .into_iter()
            .nth(*i as usize)
            .ok_or_else(|| anyhow!("找不到第 {i} 個螢幕")),
        _ => Ok(Monitor::primary()?),
    }
}

fn find_window(name: &str) -> Result<Window> {
    let needle = name.to_lowercase();
    Window::enumerate()?
        .into_iter()
        .find(|w| {
            w.is_valid()
                && (w.title().is_ok_and(|t| t.to_lowercase().contains(&needle))
                    || w.process_name().is_ok_and(|p| p.to_lowercase().contains(&needle)))
        })
        .ok_or_else(|| anyhow!("找不到「{name}」的視窗"))
}

impl Recording {
    pub fn start(opts: &RecordOptions) -> Result<Self> {
        std::fs::create_dir_all(&opts.root).with_context(|| format!("無法建立 {}", opts.root.display()))?;
        if let Some(free) = crate::disk::free_bytes(&opts.root) {
            if free < crate::common::MIN_FREE_TO_START {
                bail!("磁碟剩餘 {:.1} GB，不到 2 GB，請先清出空間", free as f64 / 1e9);
            }
        }
        let cursor = if opts.show_cursor { CursorCaptureSettings::WithCursor } else { CursorCaptureSettings::WithoutCursor };
        let interval = MinimumUpdateIntervalSettings::Custom(Duration::from_millis(1000 / opts.fps.max(1) as u64));

        enum Item {
            M(Monitor),
            W(Window),
        }
        let (item, src_w, src_h, crop) = match &opts.target {
            Target::App { name } => {
                let w = find_window(name)?;
                let (ww, wh) = (w.width()? as u32, w.height()? as u32);
                (Item::W(w), ww, wh, None)
            }
            Target::Region { x, y, w, h } => {
                let m = monitor_for(&opts.target)?;
                let (mw, mh) = (m.width()?, m.height()?);
                let crop = (*x as u32, *y as u32, (*w as u32).min(mw), (*h as u32).min(mh));
                (Item::M(m), crop.2, crop.3, Some(crop))
            }
            Target::Display { .. } => {
                let m = monitor_for(&opts.target)?;
                let (mw, mh) = (m.width()?, m.height()?);
                (Item::M(m), mw, mh, None)
            }
        };
        let (width, height) = output_size(src_w as f64, src_h as f64, opts.resolution);

        let (dir, now) = crate::common::new_session_dir(opts)?;
        let file_name = "recording.mp4";
        let sink = Sink::new(
            &dir.join(file_name),
            &SinkConfig {
                width,
                height,
                fps: opts.fps,
                codec: opts.codec,
                video_bitrate_kbps: opts.video_bitrate_kbps,
                fragment_seconds: opts.fragment_seconds,
                system_audio: opts.system_audio,
                mic: opts.mic,
            },
        )?;
        let session = crate::common::new_session(opts, now, file_name, width, height);
        session.save(&dir)?;

        let shared = Arc::new(Shared {
            sink: Mutex::new(Some(sink)),
            clock: Mutex::new(Clock { start: Instant::now(), paused_total: Duration::ZERO, paused_since: None }),
            paused: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            error: Mutex::new(None),
            last_frame: Mutex::new(None),
        });
        let flags = Flags { shared: shared.clone(), out_w: width, out_h: height, fps: opts.fps, crop };
        let started = match item {
            Item::M(m) => Capturer::start_free_threaded(Settings::new(
                m,
                cursor,
                DrawBorderSettings::Default,
                SecondaryWindowSettings::Default,
                interval,
                DirtyRegionSettings::Default,
                ColorFormat::Bgra8,
                flags,
            )),
            Item::W(w) => Capturer::start_free_threaded(Settings::new(
                w,
                cursor,
                DrawBorderSettings::Default,
                SecondaryWindowSettings::Include,
                interval,
                DirtyRegionSettings::Default,
                ColorFormat::Bgra8,
                flags,
            )),
        };
        let capture = match started {
            Ok(c) => c,
            Err(e) => {
                let mut s = session.clone();
                s.status = Status::Failed;
                s.error = Some(format!("{e}"));
                let _ = s.save(&dir);
                bail!("開始擷取畫面失敗：{e}");
            }
        };

        let mut threads = Vec::new();
        if opts.system_audio {
            threads.push(audio_thread(shared.clone(), Track::System, None));
        }
        if opts.mic {
            threads.push(audio_thread(shared.clone(), Track::Mic, opts.mic_device.clone()));
        }
        // 畫面靜止時定期補寫最後一張
        {
            let sh = shared.clone();
            threads.push(std::thread::spawn(move || {
                while !sh.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(500));
                    let stale = sh.last_frame.lock().unwrap().as_ref().filter(|(_, t)| t.elapsed() >= KEEPALIVE).map(|(f, _)| f.clone());
                    if let Some(f) = stale {
                        sh.write_video(&f);
                        if let Some(lf) = sh.last_frame.lock().unwrap().as_mut() {
                            lf.1 = Instant::now();
                        }
                    }
                }
            }));
        }
        Ok(Self { dir, shared, capture: Some(capture), threads, session })
    }

    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::SeqCst)
    }

    pub fn elapsed(&self) -> Duration {
        let c = self.shared.clock.lock().unwrap();
        Duration::from_nanos(c.now_hns() as u64 * 100)
    }

    pub fn pause(&mut self) -> Result<()> {
        if self.is_paused() {
            return Ok(());
        }
        self.shared.paused.store(true, Ordering::SeqCst);
        self.shared.clock.lock().unwrap().paused_since = Some(Instant::now());
        self.session.status = Status::Paused;
        self.session.save(&self.dir)
    }

    pub fn resume(&mut self) -> Result<()> {
        if !self.is_paused() {
            return Ok(());
        }
        {
            let mut c = self.shared.clock.lock().unwrap();
            if let Some(p) = c.paused_since.take() {
                c.paused_total += p.elapsed();
            }
            self.session.paused_seconds = c.paused_total.as_secs_f64();
        }
        self.shared.paused.store(false, Ordering::SeqCst);
        self.session.status = Status::Recording;
        self.session.save(&self.dir)
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    pub fn disk_low(&self) -> bool {
        crate::disk::free_bytes(&self.dir).is_some_and(|f| f < crate::common::MIN_FREE_WHILE_RECORDING)
    }

    pub fn stop(mut self, reason: Option<String>) -> Result<PathBuf> {
        let _ = self.resume();
        let elapsed = self.elapsed();
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(c) = self.capture.take() {
            let _ = c.stop();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let sink = self.shared.sink.lock().unwrap().take();
        let mut result = match sink {
            Some(s) => {
                self.session.frames = Some(s.counts);
                s.finish()
            }
            None => Err(anyhow!("SinkWriter 已不存在")),
        };
        if let Some(e) = self.error() {
            result = Err(anyhow!(e));
        }
        self.session.ended_at = Some(chrono::Local::now().to_rfc3339());
        self.session.duration_seconds = Some(elapsed.as_secs_f64());
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

/// 麥克風清單：(id, 名稱, 是否預設)
pub fn microphones() -> Vec<(String, String, bool)> {
    let _ = wasapi::initialize_mta();
    let Ok(en) = DeviceEnumerator::new() else { return vec![] };
    let default_id = en.get_default_device(Direction::Capture).ok().and_then(|d| d.get_id().ok());
    let Ok(col) = en.get_device_collection(Direction::Capture) else { return vec![] };
    let mut v = Vec::new();
    for d in &col {
        let Ok(d) = d else { continue };
        if let (Ok(id), Ok(name)) = (d.get_id(), d.get_friendlyname()) {
            let is_default = default_id.as_deref() == Some(id.as_str());
            v.push((id, name, is_default));
        }
    }
    v
}

/// 可錄的視窗：(程式名稱, 標題)
pub fn windows_list() -> Vec<(String, String)> {
    Window::enumerate()
        .unwrap_or_default()
        .into_iter()
        .filter(|w| w.is_valid())
        .filter_map(|w| {
            let t = w.title().ok()?;
            (!t.trim().is_empty()).then(|| (w.process_name().unwrap_or_default(), t))
        })
        .collect()
}

pub fn list_sources() -> Result<String> {
    let mut out = String::from("螢幕（--display 用序號）：\n");
    for (i, m) in Monitor::enumerate()?.iter().enumerate() {
        out.push_str(&format!("  {i}: {}x{}\n", m.width().unwrap_or(0), m.height().unwrap_or(0)));
    }
    out.push_str("視窗（--app 用程式名稱或標題部分比對）：\n");
    for (p, t) in windows_list() {
        out.push_str(&format!("  {p} — {t}\n"));
    }
    out.push_str("麥克風：\n");
    for (id, name, d) in microphones() {
        out.push_str(&format!("  {}{} id={}\n", if d { "* " } else { "  " }, name, id));
    }
    Ok(out)
}

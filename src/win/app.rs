//! Windows 系統匣 App（tray-icon）：圖示顏色表示待機／錄影中／處理中，滑鼠停留顯示計時，設定都放在右鍵選單。

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tray_icon::menu::{CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, IDOK, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_OKCANCEL, MB_SETFOREGROUND, MB_TOPMOST,
    MSG, MessageBoxW, SetTimer, TranslateMessage, WM_TIMER,
};
use windows::core::HSTRING;

use crate::common::{Codec, open_path, open_text};
use crate::config::Config;
use crate::jobs::{self, Engine, Job};
use crate::recorder::{self, Recording};
use crate::session::{Session, Status};

fn message(title: &str, body: &str) {
    unsafe {
        MessageBoxW(None, &HSTRING::from(body), &HSTRING::from(title), MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND | MB_TOPMOST);
    }
}

fn confirm(title: &str, body: &str) -> bool {
    unsafe {
        MessageBoxW(None, &HSTRING::from(body), &HSTRING::from(title), MB_OKCANCEL | MB_ICONQUESTION | MB_SETFOREGROUND | MB_TOPMOST) == IDOK
    }
}

fn hms(d: std::time::Duration) -> String {
    let t = d.as_secs();
    format!("{:02}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60)
}

/// 32×32 圓點圖示
fn dot(r: u8, g: u8, b: u8) -> Icon {
    let n = 32u32;
    let mut px = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let d = (dx * dx + dy * dy).sqrt();
            let a = ((13.0 - d).clamp(0.0, 1.0) * 255.0) as u8;
            px.extend_from_slice(&[r, g, b, a]);
        }
    }
    Icon::from_rgba(px, n, n).expect("圖示格式錯誤")
}

struct App {
    cfg: Config,
    rec: Option<Recording>,
    job: Arc<Mutex<Job>>,
    recent: Vec<PathBuf>,
    mics: Vec<(String, String, bool)>,
    windows: Vec<(String, String)>,
    tray: TrayIcon,
    /// 上次建選單時的狀態；狀態沒變就不重建，免得打開中的選單被關掉
    menu_sig: String,
}

fn item(menu: &dyn Fn(&dyn IsMenuItem), id: &str, text: &str, enabled: bool) {
    menu(&MenuItem::with_id(id, text, enabled, None));
}

fn check(menu: &dyn Fn(&dyn IsMenuItem), id: &str, text: &str, enabled: bool, checked: bool) {
    menu(&CheckMenuItem::with_id(id, text, enabled, checked, None));
}

impl App {
    fn state_sig(&self) -> String {
        let j = self.job.lock().unwrap();
        format!(
            "{}|{}|{}",
            self.rec.as_ref().map(|r| if r.is_paused() { "p" } else { "r" }).unwrap_or("-"),
            j.running.is_some(),
            toml::to_string(&self.cfg).unwrap_or_default()
        )
    }

    fn build_menu(&mut self) -> Menu {
        let menu = Menu::new();
        let add = |i: &dyn IsMenuItem| {
            let _ = menu.append(i);
        };
        let (busy, cancellable) = {
            let j = self.job.lock().unwrap();
            (j.running.clone(), j.cancellable)
        };
        match &self.rec {
            None => item(&add, "start", "開始錄影（名稱用目前時間）", true),
            Some(r) => {
                item(&add, "pause", if r.is_paused() { "繼續" } else { "暫停" }, true);
                item(&add, "stop", "停止並存檔", true);
            }
        }
        if let Some(what) = &busy {
            item(&add, "-", &format!("處理中：{what}"), false);
            if cancellable {
                item(&add, "cancel", "取消處理", true);
            }
        }
        add(&PredefinedMenuItem::separator());

        self.recent = jobs::recent_sessions(&self.cfg.output_root(), 5);
        let rs = Submenu::new("最近 5 場", true);
        if self.recent.is_empty() {
            let _ = rs.append(&MenuItem::new("（還沒有錄影）", false, None));
        }
        for (i, d) in self.recent.iter().enumerate() {
            let mut label = crate::common::dir_name(d);
            if Session::load(d).is_ok_and(|s| s.status == Status::Failed) {
                label.push_str("（未正常結束）");
            }
            if d.join("transcript.md").exists() {
                label.push_str(" ｜逐字稿");
            }
            if d.join("minutes_draft.md").exists() {
                label.push_str("、草稿");
            }
            let sub = Submenu::new(label, true);
            let sa = |x: &dyn IsMenuItem| {
                let _ = sub.append(x);
            };
            let recording_this = self.rec.as_ref().is_some_and(|r| &r.dir == d);
            item(&sa, &format!("open:{i}"), "開啟資料夾", true);
            item(&sa, &format!("local:{i}"), "產生逐字稿（本機）", busy.is_none() && self.rec.is_none());
            item(&sa, &format!("gemini:{i}"), "產生逐字稿（Gemini，上傳）", busy.is_none() && !recording_this);
            if jobs::has_failed_chunks(d) {
                item(&sa, &format!("retry:{i}"), "重跑 Gemini 失敗段落", busy.is_none());
            }
            item(&sa, &format!("minutes:{i}"), "產生會議紀錄草稿（Gemini）", busy.is_none() && d.join("transcript.md").exists());
            let _ = rs.append(&sub);
        }
        add(&rs);

        let c = &self.cfg;
        let editable = self.rec.is_none();
        let s = Submenu::new("設定", true);
        let sa = |x: &dyn IsMenuItem| {
            let _ = s.append(x);
        };
        let t = Submenu::new("擷取範圍", editable);
        let ta = |x: &dyn IsMenuItem| {
            let _ = t.append(x);
        };
        check(&ta, "target:display", "主螢幕", true, c.capture.target == "display");
        let _ = t.append(&PredefinedMenuItem::separator());
        self.windows = recorder::windows_list();
        for (i, (p, title)) in self.windows.iter().enumerate().take(20) {
            let label: String = format!("{p} — {title}").chars().take(60).collect();
            let on = c.capture.target == "window" && (c.capture.app.eq_ignore_ascii_case(p) || c.capture.app == *title);
            check(&ta, &format!("win:{i}"), &label, true, on);
        }
        sa(&t);
        let f = Submenu::new(format!("每秒張數：{}", c.capture.fps), editable);
        for v in [1, 2, 5, 15] {
            let _ = f.append(&CheckMenuItem::with_id(format!("fps:{v}"), format!("{v} 張"), true, c.capture.fps == v, None));
        }
        sa(&f);
        let r = Submenu::new(format!("解析度：{}p", c.capture.resolution), editable);
        for v in [720, 1080] {
            let _ = r.append(&CheckMenuItem::with_id(format!("res:{v}"), format!("{v}p"), true, c.capture.resolution == v, None));
        }
        sa(&r);
        let cd = Submenu::new("編碼", editable);
        let _ = cd.append(&CheckMenuItem::with_id("codec:hevc", "HEVC（檔案較小）", true, c.encode.video_codec == Codec::Hevc, None));
        let _ = cd.append(&CheckMenuItem::with_id("codec:h264", "H.264（相容性較好）", true, c.encode.video_codec == Codec::H264, None));
        sa(&cd);
        check(&sa, "toggle:sys", "錄系統聲音", editable, c.capture.capture_system_audio);
        check(&sa, "toggle:mic", "錄麥克風", editable, c.capture.capture_mic);
        check(&sa, "toggle:cursor", "錄游標", editable, c.capture.show_cursor);
        let m = Submenu::new("麥克風裝置", editable);
        let _ = m.append(&CheckMenuItem::with_id("mic:default", "系統預設", true, c.capture.mic_device == "default", None));
        self.mics = recorder::microphones();
        for (i, (id, name, _)) in self.mics.iter().enumerate() {
            let _ = m.append(&CheckMenuItem::with_id(format!("mic:{i}"), name, true, c.capture.mic_device == *id, None));
        }
        sa(&m);
        item(&sa, "open:output", &format!("開啟輸出資料夾（{}）", c.output.root), true);
        let _ = s.append(&PredefinedMenuItem::separator());
        let lm = Submenu::new(format!("本機模型：{}", c.transcribe.local.model), true);
        let _ = lm.append(&CheckMenuItem::with_id("model:large-v3-turbo", "large-v3-turbo", true, c.transcribe.local.model == "large-v3-turbo", None));
        let _ = lm.append(&CheckMenuItem::with_id("model:medium", "medium", true, c.transcribe.local.model == "medium", None));
        sa(&lm);
        item(&sa, "key", "Gemini API key（認證管理員）…", true);
        item(&sa, "glossary", "詞彙表…", true);
        item(&sa, "corrections", "誤辨對照表…", true);
        item(&sa, "config", "開啟設定檔（模型名稱、輸出資料夾等）…", true);
        item(&sa, "privacy:mic", "麥克風權限…", true);
        add(&s);
        add(&PredefinedMenuItem::separator());
        item(&add, "quit", "結束", true);
        menu
    }

    fn refresh(&mut self, force: bool) {
        let sig = self.state_sig();
        if force || sig != self.menu_sig {
            let m = self.build_menu();
            self.tray.set_menu(Some(Box::new(m)));
            self.menu_sig = sig;
            let icon = match (&self.rec, self.job.lock().unwrap().running.is_some()) {
                (Some(r), _) if r.is_paused() => dot(240, 180, 0),
                (Some(_), _) => dot(220, 30, 30),
                (None, true) => dot(30, 120, 220),
                (None, false) => dot(140, 140, 140),
            };
            let _ = self.tray.set_icon(Some(icon));
        }
        let tip = match &self.rec {
            Some(r) if r.is_paused() => format!("會議錄影：已暫停 {}", hms(r.elapsed())),
            Some(r) => format!("會議錄影：錄影中 {}", hms(r.elapsed())),
            None => match &self.job.lock().unwrap().running {
                Some(w) => format!("會議錄影：{w}"),
                None => "會議錄影：待機".into(),
            },
        };
        let _ = self.tray.set_tooltip(Some(tip));
    }

    fn save_cfg(&mut self, f: impl FnOnce(&mut Config)) {
        f(&mut self.cfg);
        if let Err(e) = self.cfg.save() {
            message("設定存檔失敗", &format!("{e:#}"));
        }
    }

    fn stop(&mut self, reason: Option<String>) {
        let Some(r) = self.rec.take() else { return };
        match r.stop(reason.clone()) {
            Ok(dir) => {
                let body = format!("{}{}\n\n要開啟資料夾嗎？", reason.map(|r| format!("{r}\n")).unwrap_or_default(), dir.display());
                if confirm("錄影已存檔", &body) {
                    open_path(&dir);
                }
            }
            Err(e) => message("存檔時發生錯誤", &format!("{e:#}\n已錄的內容（fragment）通常仍可播放，請檢查資料夾。")),
        }
    }

    fn confirm_upload(&self, what: &str) -> bool {
        confirm(
            "會議資料將上傳至 Google",
            &format!(
                "{what}\n\n模型：{}\n請確認使用的是付費方案的 API key（付費方案的內容不會被用來改進 Google 產品）。這個動作只對這一場有效。",
                self.cfg.transcribe.gemini.model
            ),
        )
    }

    fn handle(&mut self, id: &str) -> bool {
        let (cmd, arg) = id.split_once(':').unwrap_or((id, ""));
        let idx: Option<usize> = arg.parse().ok();
        let recent = |i: Option<usize>| i.and_then(|i| self.recent.get(i).cloned());
        match cmd {
            "start" if self.rec.is_none() => {
                let mut o = crate::record_options_from_config(&self.cfg);
                o.name = String::new();
                match Recording::start(&o) {
                    Ok(r) => self.rec = Some(r),
                    Err(e) => message("無法開始錄影", &format!("{e:#}")),
                }
            }
            "pause" => {
                if let Some(r) = self.rec.as_mut() {
                    let res = if r.is_paused() { r.resume() } else { r.pause() };
                    if let Err(e) = res {
                        message("暫停／繼續失敗", &format!("{e:#}"));
                    }
                }
            }
            "stop" => self.stop(None),
            "cancel" => self.job.lock().unwrap().cancel.store(true, Ordering::SeqCst),
            "open" if arg == "output" => {
                let root = self.cfg.output_root();
                let _ = std::fs::create_dir_all(&root);
                open_path(&root);
            }
            "open" => {
                if let Some(d) = recent(idx) {
                    open_path(&d);
                }
            }
            "local" => {
                if let Some(d) = recent(idx) {
                    jobs::spawn_transcribe(self.job.clone(), d, Engine::Local, false, self.cfg.clone());
                }
            }
            "gemini" | "retry" => {
                if let Some(d) = recent(idx) {
                    let what = if cmd == "retry" { "上次失敗的段落會重新上傳到 Google（Gemini API）。" } else { "這場的 16 kHz 音檔會上傳到 Google（Gemini API）。處理完每一段會立刻刪除上傳檔。" };
                    if self.confirm_upload(what) {
                        jobs::spawn_transcribe(self.job.clone(), d, Engine::Gemini, cmd == "retry", self.cfg.clone());
                    }
                }
            }
            "minutes" => {
                if let Some(d) = recent(idx) {
                    if self.confirm_upload("這場的逐字稿（transcript.md）會送到 Google（Gemini API）產生會議紀錄草稿。") {
                        jobs::spawn_minutes(self.job.clone(), d, self.cfg.clone());
                    }
                }
            }
            "target" => self.save_cfg(|c| c.capture.target = "display".into()),
            "win" => {
                if let Some((p, t)) = idx.and_then(|i| self.windows.get(i).cloned()) {
                    let name = if p.is_empty() { t } else { p };
                    self.save_cfg(|c| {
                        c.capture.target = "window".into();
                        c.capture.app = name;
                    });
                }
            }
            "fps" => {
                if let Some(v) = idx {
                    self.save_cfg(|c| c.capture.fps = v as u32);
                }
            }
            "res" => {
                if let Some(v) = idx {
                    self.save_cfg(|c| c.capture.resolution = v as u32);
                }
            }
            "codec" => self.save_cfg(|c| c.encode.video_codec = if arg == "h264" { Codec::H264 } else { Codec::Hevc }),
            "toggle" => self.save_cfg(|c| match arg {
                "sys" => c.capture.capture_system_audio = !c.capture.capture_system_audio,
                "mic" => c.capture.capture_mic = !c.capture.capture_mic,
                _ => c.capture.show_cursor = !c.capture.show_cursor,
            }),
            "mic" => {
                let id = if arg == "default" { "default".to_string() } else { idx.and_then(|i| self.mics.get(i)).map(|m| m.0.clone()).unwrap_or("default".into()) };
                self.save_cfg(|c| c.capture.mic_device = id);
            }
            "model" => {
                if crate::local_asr::model_path(arg).is_err() {
                    message("模型尚未下載", &format!("請在終端機執行：\nmeeting-recorder models download {arg}\nmeeting-recorder models download silero-vad"));
                }
                let a = arg.to_string();
                self.save_cfg(|c| c.transcribe.local.model = a);
            }
            "key" => message(
                "Gemini API key",
                "請在「終端機」執行：\n\nmeeting-recorder gemini set-key\n\n貼上 key 後按 Enter，key 只會存進 Windows 認證管理員。請使用有啟用付費（billing）的專案金鑰。",
            ),
            "glossary" => {
                crate::transcript::load_glossary();
                open_text(&crate::common::app_support().join("glossary.txt"));
            }
            "corrections" => {
                crate::transcript::load_corrections();
                open_text(&crate::common::app_support().join("corrections.txt"));
            }
            "config" => {
                open_text(&crate::config::path());
                // 記事本關掉後重新讀設定
                if let Ok(c) = Config::load() {
                    self.cfg = c;
                }
            }
            "privacy" => {
                let _ = std::process::Command::new("explorer.exe").arg("ms-settings:privacy-microphone").status();
            }
            "quit" => {
                if self.rec.is_some() {
                    if !confirm("還在錄影", "要停止錄影、存檔並結束嗎？") {
                        return false;
                    }
                    self.stop(None);
                }
                return true;
            }
            _ => {}
        }
        false
    }

    fn tick(&mut self) {
        let mut reason = None;
        if let Some(r) = &self.rec {
            if let Some(e) = r.error() {
                reason = Some(format!("錄影發生錯誤，已停止：{e}"));
            } else if r.disk_low() {
                reason = Some("磁碟剩餘不到 500 MB，已自動停止並存檔".into());
            }
        }
        if let Some(r) = reason {
            self.stop(Some(r));
        }
        let notice = self.job.lock().unwrap().notice.take();
        self.refresh(notice.is_some());
        if let Some((title, body, dir)) = notice {
            match dir {
                Some(d) if confirm(&title, &format!("{body}\n\n要開啟資料夾嗎？")) => open_path(&d),
                Some(_) => {}
                None => message(&title, &body),
            }
        }
    }
}

pub fn run() {
    if let Err(e) = run_inner() {
        message("會議錄影", &format!("無法啟動：{e:#}"));
    }
}

fn run_inner() -> Result<()> {
    let cfg = Config::load().unwrap_or_default();
    let tray = TrayIconBuilder::new().with_icon(dot(140, 140, 140)).with_tooltip("會議錄影：待機").build()?;
    let mut app = App {
        cfg,
        rec: None,
        job: Arc::new(Mutex::new(Job::default())),
        recent: vec![],
        mics: vec![],
        windows: vec![],
        tray,
        menu_sig: String::new(),
    };
    app.refresh(true);
    for d in jobs::mark_unfinished(&app.cfg.output_root()) {
        if confirm(
            "上一場錄影沒有正常結束",
            &format!("{}\n\n錄影檔以 fragment 分段寫入，已錄的部分通常仍可播放。要開啟資料夾檢查嗎？", d.display()),
        ) {
            open_path(&d);
        }
    }
    unsafe { SetTimer(None, 1, 1000, None) };
    let mut msg = MSG::default();
    loop {
        if !unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if app.handle(ev.id.as_ref()) {
                return Ok(());
            }
            app.refresh(true);
        }
        if msg.message == WM_TIMER {
            app.tick();
        }
    }
    Ok(())
}

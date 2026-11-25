//! 選單列 App（objc2-app-kit，不用 Tauri）：待機／錄影中／處理中三種狀態，設定都放在子選單。

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSApplication, NSApplicationActivationPolicy, NSControlStateValueOff,
    NSControlStateValueOn, NSMenu, NSMenuDelegate, NSMenuItem, NSModalResponseOK, NSOpenPanel, NSSecureTextField, NSStatusBar,
    NSStatusItem, NSTextField, NSVariableStatusItemLength, NSView, NSWorkspace,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSTimer, NSURL};
use screencapturekit::prelude::AudioInputDevice;

use crate::config::Config;
use crate::jobs::{self, Engine, Job};
use crate::common::{RecordOptions, Target};
use crate::recorder::Recording;
use crate::session::{Session, Status};
use crate::common::Codec;

const SCREEN_PRIVACY_URL: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
const MIC_PRIVACY_URL: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone";

pub struct Ivars {
    cfg: RefCell<Config>,
    rec: RefCell<Option<Recording>>,
    item: RefCell<Option<Retained<NSStatusItem>>>,
    job: Arc<Mutex<Job>>,
    recent: RefCell<Vec<PathBuf>>,
    mics: RefCell<Vec<AudioInputDevice>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MRAppController"]
    #[ivars = Ivars]
    pub struct Controller;

    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl NSMenuDelegate for Controller {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            self.build_menu(menu);
        }
    }

    impl Controller {
        #[unsafe(method(startRecording:))]
        fn start_recording(&self, _s: Option<&AnyObject>) {
            self.do_start();
        }
        #[unsafe(method(pauseResume:))]
        fn pause_resume(&self, _s: Option<&AnyObject>) {
            let mut rec = self.ivars().rec.borrow_mut();
            if let Some(r) = rec.as_mut() {
                let res = if r.is_paused() { r.resume() } else { r.pause() };
                if let Err(e) = res {
                    drop(rec);
                    self.alert("暫停／繼續失敗", &format!("{e:#}"));
                }
            }
            self.refresh_title();
        }
        #[unsafe(method(stopRecording:))]
        fn stop_recording(&self, _s: Option<&AnyObject>) {
            self.do_stop(None);
        }
        #[unsafe(method(openSession:))]
        fn open_session(&self, s: Option<&AnyObject>) {
            if let Some(d) = self.recent_at(s) {
                open_path(&d);
            }
        }
        #[unsafe(method(transcribeLocal:))]
        fn transcribe_local(&self, s: Option<&AnyObject>) {
            if self.ivars().rec.borrow().is_some() {
                self.alert("錄影中不能跑本機辨識", "本機辨識會和錄影搶資源，請停止錄影後再執行。");
                return;
            }
            if let Some(d) = self.recent_at(s) {
                self.spawn_transcribe(d, Engine::Local, false);
            }
        }
        #[unsafe(method(transcribeGemini:))]
        fn transcribe_gemini(&self, s: Option<&AnyObject>) {
            let Some(d) = self.recent_at(s) else { return };
            if self.confirm_upload("這場的 16 kHz 音檔會上傳到 Google（Gemini API）。處理完每一段會立刻刪除上傳檔。") {
                self.spawn_transcribe(d, Engine::Gemini, false);
            }
        }
        #[unsafe(method(retryGemini:))]
        fn retry_gemini(&self, s: Option<&AnyObject>) {
            let Some(d) = self.recent_at(s) else { return };
            if self.confirm_upload("上次失敗的段落會重新上傳到 Google（Gemini API）。") {
                self.spawn_transcribe(d, Engine::Gemini, true);
            }
        }
        #[unsafe(method(makeMinutes:))]
        fn make_minutes(&self, s: Option<&AnyObject>) {
            let Some(d) = self.recent_at(s) else { return };
            if !d.join("transcript.md").exists() {
                self.alert("還沒有逐字稿", "請先產生逐字稿（本機或 Gemini）。");
                return;
            }
            if self.confirm_upload("這場的逐字稿（transcript.md）會送到 Google（Gemini API）產生會議紀錄草稿。") {
                self.spawn_minutes(d);
            }
        }
        #[unsafe(method(cancelJob:))]
        fn cancel_job(&self, _s: Option<&AnyObject>) {
            self.ivars().job.lock().unwrap().cancel.store(true, Ordering::SeqCst);
        }
        #[unsafe(method(setFps:))]
        fn set_fps(&self, s: Option<&AnyObject>) {
            let v = tag(s) as u32;
            self.update_cfg(|c| c.capture.fps = v);
        }
        #[unsafe(method(setRes:))]
        fn set_res(&self, s: Option<&AnyObject>) {
            let v = tag(s) as u32;
            self.update_cfg(|c| c.capture.resolution = v);
        }
        #[unsafe(method(setCodec:))]
        fn set_codec(&self, s: Option<&AnyObject>) {
            let v = if tag(s) == 1 { Codec::H264 } else { Codec::Hevc };
            self.update_cfg(|c| c.encode.video_codec = v);
        }
        #[unsafe(method(targetDisplay:))]
        fn target_display(&self, _s: Option<&AnyObject>) {
            self.update_cfg(|c| c.capture.target = "display".into());
        }
        #[unsafe(method(targetApp:))]
        fn target_app(&self, _s: Option<&AnyObject>) {
            let cur = self.ivars().cfg.borrow().capture.app.clone();
            if let Some(v) = self.prompt("擷取單一視窗", "輸入 App 名稱（部分比對即可，例如 Teams、Meet、Chrome）：", &cur, false) {
                if !v.trim().is_empty() {
                    self.update_cfg(|c| {
                        c.capture.target = "window".into();
                        c.capture.app = v.trim().into();
                    });
                }
            }
        }
        #[unsafe(method(targetRegion:))]
        fn target_region(&self, _s: Option<&AnyObject>) {
            let r = self.ivars().cfg.borrow().capture.region;
            let cur = format!("{},{},{},{}", r[0], r[1], r[2], r[3]);
            if let Some(v) = self.prompt("擷取指定區域", "主螢幕上的區域 x,y,寬,高（點座標，左上角為 0,0）：", &cur, false) {
                let p: Vec<f64> = v.split(',').filter_map(|s| s.trim().parse().ok()).collect();
                if let [x, y, w, h] = p[..] {
                    if w > 0.0 && h > 0.0 {
                        self.update_cfg(|c| {
                            c.capture.target = "region".into();
                            c.capture.region = [x, y, w, h];
                        });
                        return;
                    }
                }
                self.alert("格式不對", "請輸入四個數字，例如 0,0,1280,720");
            }
        }
        #[unsafe(method(toggleSystemAudio:))]
        fn toggle_system_audio(&self, _s: Option<&AnyObject>) {
            self.update_cfg(|c| c.capture.capture_system_audio = !c.capture.capture_system_audio);
        }
        #[unsafe(method(toggleMic:))]
        fn toggle_mic(&self, _s: Option<&AnyObject>) {
            self.update_cfg(|c| c.capture.capture_mic = !c.capture.capture_mic);
        }
        #[unsafe(method(toggleCursor:))]
        fn toggle_cursor(&self, _s: Option<&AnyObject>) {
            self.update_cfg(|c| c.capture.show_cursor = !c.capture.show_cursor);
        }
        #[unsafe(method(setMic:))]
        fn set_mic(&self, s: Option<&AnyObject>) {
            let i = tag(s);
            let id = if i < 0 { "default".to_string() } else { self.ivars().mics.borrow().get(i as usize).map(|m| m.id.clone()).unwrap_or("default".into()) };
            self.update_cfg(|c| c.capture.mic_device = id);
        }
        #[unsafe(method(chooseOutput:))]
        fn choose_output(&self, _s: Option<&AnyObject>) {
            let mtm = self.mtm();
            activate(mtm);
            let p = NSOpenPanel::openPanel(mtm);
            p.setCanChooseDirectories(true);
            p.setCanChooseFiles(false);
            p.setCanCreateDirectories(true);
            p.setPrompt(Some(&NSString::from_str("選擇")));
            if p.runModal() == NSModalResponseOK {
                if let Some(path) = p.URL().and_then(|u| u.to_file_path()) {
                    self.update_cfg(|c| c.output.root = path.to_string_lossy().into_owned());
                }
            }
        }
        #[unsafe(method(setLocalModel:))]
        fn set_local_model(&self, s: Option<&AnyObject>) {
            let name = if tag(s) == 1 { "medium" } else { "large-v3-turbo" };
            if crate::local_asr::model_path(name).is_err() {
                self.alert("模型尚未下載", &format!("請在終端機執行：\nmeeting-recorder models download {name}"));
            }
            self.update_cfg(|c| c.transcribe.local.model = name.into());
        }
        #[unsafe(method(setGeminiModel:))]
        fn set_gemini_model(&self, _s: Option<&AnyObject>) {
            let cur = self.ivars().cfg.borrow().transcribe.gemini.model.clone();
            if let Some(v) = self.prompt("Gemini 模型名稱", "依 Gemini 官方模型頁填入（需支援音訊輸入）：", &cur, false) {
                if !v.trim().is_empty() {
                    self.update_cfg(|c| c.transcribe.gemini.model = v.trim().into());
                }
            }
        }
        #[unsafe(method(setGeminiKey:))]
        fn set_gemini_key(&self, _s: Option<&AnyObject>) {
            let has = crate::keychain::gemini_key().is_ok();
            let msg = if has { "Keychain 已有 key；輸入新的會覆蓋。請使用有啟用付費（billing）的專案金鑰。" } else { "只會存進 macOS Keychain。請使用有啟用付費（billing）的專案金鑰。" };
            if let Some(v) = self.prompt("Gemini API key", msg, "", true) {
                if !v.trim().is_empty() {
                    match crate::keychain::set_gemini_key(&v) {
                        Ok(()) => self.alert("已儲存", "Gemini API key 已存入 Keychain。"),
                        Err(e) => self.alert("儲存失敗", &format!("{e:#}")),
                    }
                }
            }
        }
        #[unsafe(method(editGlossary:))]
        fn edit_glossary(&self, _s: Option<&AnyObject>) {
            crate::transcript::load_glossary();
            open_in_textedit(&crate::transcript::app_support().join("glossary.txt"));
        }
        #[unsafe(method(editCorrections:))]
        fn edit_corrections(&self, _s: Option<&AnyObject>) {
            crate::transcript::load_corrections();
            open_in_textedit(&crate::transcript::app_support().join("corrections.txt"));
        }
        #[unsafe(method(openConfig:))]
        fn open_config(&self, _s: Option<&AnyObject>) {
            open_in_textedit(&crate::config::path());
        }
        #[unsafe(method(openPrivacy:))]
        fn open_privacy(&self, s: Option<&AnyObject>) {
            open_url(if tag(s) == 1 { MIC_PRIVACY_URL } else { SCREEN_PRIVACY_URL });
        }
        #[unsafe(method(quit:))]
        fn quit(&self, _s: Option<&AnyObject>) {
            if self.ivars().rec.borrow().is_some() {
                if !self.confirm("還在錄影", "要停止錄影、存檔並結束嗎？", "停止並結束") {
                    return;
                }
                self.do_stop(None);
            }
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }
    }
);

fn tag(s: Option<&AnyObject>) -> isize {
    s.map(|o| {
        let item: &NSMenuItem = unsafe { &*(o as *const AnyObject as *const NSMenuItem) };
        item.tag()
    })
    .unwrap_or(0)
}

fn activate(mtm: MainThreadMarker) {
    NSApplication::sharedApplication(mtm).activate();
}

fn open_url(u: &str) {
    if let Some(url) = NSURL::URLWithString(&NSString::from_str(u)) {
        NSWorkspace::sharedWorkspace().openURL(&url);
    }
}

use crate::common::{open_path, open_text as open_in_textedit};

fn hms(d: std::time::Duration) -> String {
    let t = d.as_secs();
    format!("{:02}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60)
}

impl Controller {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let cfg = Config::load().unwrap_or_default();
        let this = Self::alloc(mtm).set_ivars(Ivars {
            cfg: RefCell::new(cfg),
            rec: RefCell::new(None),
            item: RefCell::new(None),
            job: Arc::new(Mutex::new(Job::default())),
            recent: RefCell::new(vec![]),
            mics: RefCell::new(vec![]),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn update_cfg(&self, f: impl FnOnce(&mut Config)) {
        let mut c = self.ivars().cfg.borrow_mut();
        f(&mut c);
        if let Err(e) = c.save() {
            drop(c);
            self.alert("設定存檔失敗", &format!("{e:#}"));
        }
    }

    fn recent_at(&self, s: Option<&AnyObject>) -> Option<PathBuf> {
        self.ivars().recent.borrow().get(tag(s) as usize).cloned()
    }

    fn alert(&self, title: &str, body: &str) {
        let mtm = self.mtm();
        activate(mtm);
        let a = NSAlert::new(mtm);
        a.setMessageText(&NSString::from_str(title));
        a.setInformativeText(&NSString::from_str(body));
        a.runModal();
    }

    fn confirm(&self, title: &str, body: &str, ok: &str) -> bool {
        let mtm = self.mtm();
        activate(mtm);
        let a = NSAlert::new(mtm);
        a.setMessageText(&NSString::from_str(title));
        a.setInformativeText(&NSString::from_str(body));
        a.addButtonWithTitle(&NSString::from_str(ok));
        a.addButtonWithTitle(&NSString::from_str("取消"));
        a.runModal() == NSAlertFirstButtonReturn
    }

    fn confirm_upload(&self, what: &str) -> bool {
        let model = self.ivars().cfg.borrow().transcribe.gemini.model.clone();
        self.confirm(
            "會議資料將上傳至 Google",
            &format!("{what}\n\n模型：{model}\n請確認使用的是付費方案的 API key（付費方案的內容不會被用來改進 Google 產品）。這個動作只對這一場有效。"),
            "確定上傳",
        )
    }

    fn prompt(&self, title: &str, body: &str, initial: &str, secure: bool) -> Option<String> {
        let mtm = self.mtm();
        activate(mtm);
        let a = NSAlert::new(mtm);
        a.setMessageText(&NSString::from_str(title));
        a.setInformativeText(&NSString::from_str(body));
        a.addButtonWithTitle(&NSString::from_str("確定"));
        a.addButtonWithTitle(&NSString::from_str("取消"));
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(320.0, 24.0));
        let field: Retained<NSTextField> = if secure {
            let f = NSSecureTextField::initWithFrame(NSSecureTextField::alloc(mtm), frame);
            Retained::into_super(f)
        } else {
            NSTextField::initWithFrame(NSTextField::alloc(mtm), frame)
        };
        field.setStringValue(&NSString::from_str(initial));
        let view: &NSView = &field;
        a.setAccessoryView(Some(view));
        a.window().setInitialFirstResponder(Some(view));
        if a.runModal() == NSAlertFirstButtonReturn {
            Some(field.stringValue().to_string())
        } else {
            None
        }
    }

    fn add(&self, menu: &NSMenu, title: &str, action: Option<Sel>, tag: isize, checked: bool, enabled: bool) -> Retained<NSMenuItem> {
        let mtm = self.mtm();
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::from_str(title), action, &NSString::from_str(""))
        };
        if action.is_some() {
            unsafe { item.setTarget(Some(self)) };
        }
        item.setTag(tag);
        item.setState(if checked { NSControlStateValueOn } else { NSControlStateValueOff });
        item.setEnabled(enabled && action.is_some());
        menu.addItem(&item);
        item
    }

    fn submenu(&self, menu: &NSMenu, title: &str) -> Retained<NSMenu> {
        let mtm = self.mtm();
        let item = self.add(menu, title, None, 0, false, true);
        item.setEnabled(true);
        let sub = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
        sub.setAutoenablesItems(false);
        item.setSubmenu(Some(&sub));
        sub
    }

    fn build_menu(&self, menu: &NSMenu) {
        let mtm = self.mtm();
        menu.removeAllItems();
        menu.setAutoenablesItems(false);
        let cfg = self.ivars().cfg.borrow().clone();
        let rec = self.ivars().rec.borrow();
        let job = self.ivars().job.lock().unwrap();
        let busy = job.running.is_some();

        // 錄影控制
        match rec.as_ref() {
            None => {
                self.add(menu, "開始錄影…", Some(sel!(startRecording:)), 0, false, true);
            }
            Some(r) => {
                let state = if r.is_paused() { "已暫停" } else { "錄影中" };
                self.add(menu, &format!("{state} {}", hms(r.elapsed())), None, 0, false, false);
                self.add(menu, if r.is_paused() { "繼續" } else { "暫停" }, Some(sel!(pauseResume:)), 0, false, true);
                self.add(menu, "停止並存檔", Some(sel!(stopRecording:)), 0, false, true);
            }
        }
        if let Some(what) = &job.running {
            self.add(menu, &format!("處理中：{what}（{}）", job.progress), None, 0, false, false);
            if job.cancellable {
                self.add(menu, "取消處理", Some(sel!(cancelJob:)), 0, false, true);
            }
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));

        // 最近 5 場
        let recent = jobs::recent_sessions(&cfg.output_root(), 5);
        let rsub = self.submenu(menu, "最近 5 場");
        if recent.is_empty() {
            self.add(&rsub, "（還沒有錄影）", None, 0, false, false);
        }
        for (i, d) in recent.iter().enumerate() {
            let s = Session::load(d).ok();
            let mut label = crate::common::dir_name(d);
            if let Some(s) = &s {
                if s.status == Status::Failed {
                    label.push_str("（未正常結束）");
                }
            }
            let flags: Vec<&str> = [("transcript.md", "逐字稿"), ("minutes_draft.md", "草稿")]
                .iter()
                .filter(|(f, _)| d.join(f).exists())
                .map(|(_, l)| *l)
                .collect();
            if !flags.is_empty() {
                label.push_str(&format!(" ｜{}", flags.join("、")));
            }
            let sub = self.submenu(&rsub, &label);
            let recording_this = rec.as_ref().is_some_and(|r| &r.dir == d);
            let i = i as isize;
            self.add(&sub, "開啟資料夾", Some(sel!(openSession:)), i, false, true);
            self.add(&sub, "產生逐字稿（本機）", Some(sel!(transcribeLocal:)), i, false, !busy && !recording_this && rec.is_none());
            self.add(&sub, "產生逐字稿（Gemini，上傳）", Some(sel!(transcribeGemini:)), i, false, !busy && !recording_this);
            let has_failed = jobs::has_failed_chunks(d);
            if has_failed {
                self.add(&sub, "重跑 Gemini 失敗段落", Some(sel!(retryGemini:)), i, false, !busy);
            }
            self.add(&sub, "產生會議紀錄草稿（Gemini）", Some(sel!(makeMinutes:)), i, false, !busy && d.join("transcript.md").exists());
        }
        *self.ivars().recent.borrow_mut() = recent;
        drop(job);

        // 設定
        let s = self.submenu(menu, "設定");
        let editable = rec.is_none();
        let t = self.submenu(&s, "擷取範圍");
        self.add(&t, "整個螢幕", Some(sel!(targetDisplay:)), 0, cfg.capture.target == "display", editable);
        let app_label = if cfg.capture.app.is_empty() { "單一視窗…".to_string() } else { format!("單一視窗：{}…", cfg.capture.app) };
        self.add(&t, &app_label, Some(sel!(targetApp:)), 0, cfg.capture.target == "window", editable);
        let r = cfg.capture.region;
        self.add(&t, &format!("指定區域：{}×{}…", r[2], r[3]), Some(sel!(targetRegion:)), 0, cfg.capture.target == "region", editable);
        let f = self.submenu(&s, &format!("每秒張數：{}", cfg.capture.fps));
        for v in [1, 2, 5, 15] {
            self.add(&f, &format!("{v} 張"), Some(sel!(setFps:)), v, cfg.capture.fps == v as u32, editable);
        }
        let rs = self.submenu(&s, &format!("解析度：{}p", cfg.capture.resolution));
        for v in [720, 1080] {
            self.add(&rs, &format!("{v}p"), Some(sel!(setRes:)), v, cfg.capture.resolution == v as u32, editable);
        }
        let c = self.submenu(&s, "編碼");
        self.add(&c, "HEVC（檔案較小）", Some(sel!(setCodec:)), 0, cfg.encode.video_codec == Codec::Hevc, editable);
        self.add(&c, "H.264（相容性較好）", Some(sel!(setCodec:)), 1, cfg.encode.video_codec == Codec::H264, editable);
        self.add(&s, "錄系統聲音", Some(sel!(toggleSystemAudio:)), 0, cfg.capture.capture_system_audio, editable);
        self.add(&s, "錄麥克風", Some(sel!(toggleMic:)), 0, cfg.capture.capture_mic, editable);
        self.add(&s, "錄游標", Some(sel!(toggleCursor:)), 0, cfg.capture.show_cursor, editable);
        let m = self.submenu(&s, "麥克風裝置");
        let mics = AudioInputDevice::list();
        self.add(&m, "系統預設", Some(sel!(setMic:)), -1, cfg.capture.mic_device == "default", editable);
        for (i, d) in mics.iter().enumerate() {
            self.add(&m, &d.name, Some(sel!(setMic:)), i as isize, cfg.capture.mic_device == d.id, editable);
        }
        *self.ivars().mics.borrow_mut() = mics;
        self.add(&s, &format!("輸出資料夾：{}…", cfg.output.root), Some(sel!(chooseOutput:)), 0, false, editable);
        s.addItem(&NSMenuItem::separatorItem(mtm));
        let lm = self.submenu(&s, &format!("本機模型：{}", cfg.transcribe.local.model));
        self.add(&lm, "large-v3-turbo", Some(sel!(setLocalModel:)), 0, cfg.transcribe.local.model == "large-v3-turbo", true);
        self.add(&lm, "medium", Some(sel!(setLocalModel:)), 1, cfg.transcribe.local.model == "medium", true);
        self.add(&s, &format!("Gemini 模型：{}…", cfg.transcribe.gemini.model), Some(sel!(setGeminiModel:)), 0, false, true);
        self.add(&s, "Gemini API key（Keychain）…", Some(sel!(setGeminiKey:)), 0, false, true);
        self.add(&s, "詞彙表…", Some(sel!(editGlossary:)), 0, false, true);
        self.add(&s, "誤辨對照表…", Some(sel!(editCorrections:)), 0, false, true);
        self.add(&s, "開啟設定檔…", Some(sel!(openConfig:)), 0, false, true);
        s.addItem(&NSMenuItem::separatorItem(mtm));
        self.add(&s, "螢幕錄影權限…", Some(sel!(openPrivacy:)), 0, false, true);
        self.add(&s, "麥克風權限…", Some(sel!(openPrivacy:)), 1, false, true);

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        self.add(menu, "結束", Some(sel!(quit:)), 0, false, true);
    }

    fn set_title(&self, t: &str) {
        let mtm = self.mtm();
        if let Some(item) = self.ivars().item.borrow().as_ref() {
            if let Some(b) = item.button(mtm) {
                b.setTitle(&NSString::from_str(t));
            }
        }
    }

    fn refresh_title(&self) {
        let rec = self.ivars().rec.borrow();
        let job = self.ivars().job.lock().unwrap();
        let t = match (rec.as_ref(), &job.running) {
            (Some(r), _) if r.is_paused() => format!("⏸ {}", hms(r.elapsed())),
            (Some(r), _) => format!("🔴 {}", hms(r.elapsed())),
            (None, Some(_)) => format!("⏳ {}", job.progress),
            (None, None) => "⏺".to_string(),
        };
        drop(job);
        drop(rec);
        self.set_title(&t);
    }

    /// 每秒一次：更新標題、檢查錄影錯誤與磁碟空間、顯示背景工作結果
    fn tick(&self) {
        let mut stop_reason = None;
        if let Some(r) = self.ivars().rec.borrow().as_ref() {
            if let Some(e) = r.error() {
                stop_reason = Some(format!("錄影發生錯誤，已停止：{e}"));
            } else if r.disk_low() {
                stop_reason = Some("磁碟剩餘不到 500 MB，已自動停止並存檔".to_string());
            }
        }
        if let Some(reason) = stop_reason {
            self.do_stop(Some(reason));
        }
        let notice = self.ivars().job.lock().unwrap().notice.take();
        self.refresh_title();
        if let Some((title, body, dir)) = notice {
            if let Some(d) = dir {
                if self.confirm(&title, &body, "開啟資料夾") {
                    open_path(&d);
                }
            } else {
                self.alert(&title, &body);
            }
        }
    }

    fn do_start(&self) {
        if self.ivars().rec.borrow().is_some() {
            return;
        }
        let Some(name) = self.prompt("開始錄影", "會議名稱（可留空，預設用時間）：", "", false) else { return };
        let cfg = self.ivars().cfg.borrow().clone();
        let mut o: RecordOptions = crate::record_options_from_config(&cfg);
        o.name = name.trim().to_string();
        if let Target::App { .. } = o.target {
            // 視窗模式的 App 名稱已在設定裡
        }
        match Recording::start(&o) {
            Ok(r) => {
                *self.ivars().rec.borrow_mut() = Some(r);
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("TCC") || msg.contains("權限") {
                    if self.confirm(
                        "需要「螢幕與系統錄音」權限",
                        "請在系統設定 → 隱私權與安全性 → 螢幕與系統錄音 裡打開「MeetingRecorder」，然後重新開啟本 App。",
                        "開啟系統設定",
                    ) {
                        open_url(SCREEN_PRIVACY_URL);
                    }
                } else {
                    self.alert("無法開始錄影", &msg);
                }
            }
        }
        self.refresh_title();
    }

    fn do_stop(&self, reason: Option<String>) {
        let Some(r) = self.ivars().rec.borrow_mut().take() else { return };
        self.set_title("⏳ 存檔中");
        let res = r.stop(reason.clone());
        self.refresh_title();
        match res {
            Ok(dir) => {
                let body = match reason {
                    Some(r) => format!("{r}\n{}", dir.display()),
                    None => dir.display().to_string(),
                };
                if self.confirm("錄影已存檔", &body, "開啟資料夾") {
                    open_path(&dir);
                }
            }
            Err(e) => self.alert("存檔時發生錯誤", &format!("{e:#}\n已錄的內容（fragment）通常仍可播放，請檢查資料夾。")),
        }
    }

    fn spawn_transcribe(&self, dir: PathBuf, engine: Engine, retry: bool) {
        let cfg = self.ivars().cfg.borrow().clone();
        jobs::spawn_transcribe(self.ivars().job.clone(), dir, engine, retry, cfg);
        self.refresh_title();
    }

    fn spawn_minutes(&self, dir: PathBuf) {
        let cfg = self.ivars().cfg.borrow().clone();
        jobs::spawn_minutes(self.ivars().job.clone(), dir, cfg);
        self.refresh_title();
    }

    /// 啟動時檢查上一場是否沒有正常結束（App 當掉、強制結束）
    fn check_unfinished(&self) {
        let root = self.ivars().cfg.borrow().output_root();
        for d in jobs::mark_unfinished(&root) {
            if self.confirm(
                "上一場錄影沒有正常結束",
                &format!("{}\n\n錄影檔以 10 秒為單位分段寫入，已錄的部分通常仍可播放（最多少最後一段）。請開啟檢查。", d.display()),
                "開啟資料夾",
            ) {
                open_path(&d);
            }
        }
    }
}

pub fn run() {
    let mtm = MainThreadMarker::new().expect("必須在主執行緒執行");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let ctl = Controller::new(mtm);
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("MeetingRecorder"));
    menu.setDelegate(Some(ProtocolObject::from_ref(&*ctl)));
    item.setMenu(Some(&menu));
    *ctl.ivars().item.borrow_mut() = Some(item);
    ctl.refresh_title();

    let c2 = ctl.clone();
    let block = RcBlock::new(move |_t: std::ptr::NonNull<NSTimer>| c2.tick());
    let _timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0, true, &block) };

    // 等 run loop 起來後再檢查上一場（alert 需要 App 已在跑）
    let c3 = ctl.clone();
    let once = RcBlock::new(move |_t: std::ptr::NonNull<NSTimer>| c3.check_unfinished());
    let _t2 = unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.5, false, &once) };

    let _ = NSAlertSecondButtonReturn;
    app.run();
}

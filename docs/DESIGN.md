# 會議錄影逐字稿 App 設計（macOS Apple Silicon，Rust）

- 版本：v0.2（2026-11-22）
- 狀態：M0 完成；M1～M5 程式已寫好，M1 實錄與 M4／M5 實際呼叫 Gemini 尚待驗證（見第 11、13 節）
- 用途：例行會議的畫面錄影、錄音、逐字稿，取代 OBS

---

## 0. 給接手 agent 的指示（先讀）

1. **專案放哪裡還沒決定。開始實作前要先問使用者**，不要自己選路徑建立專案。
2. 下面標 `(inferred: 未實查)` 的地方是設計時沒有實際確認過的，包含 crate 版本、API 和 Gemini 規格。動工前要逐條查官方文件或 crates.io 確認。查到和本文不一樣時，以查到的為準，並回頭更新本文第 11 節。
3. Gemini API key 只放 macOS Keychain。不能寫進原始碼、設定檔、log，也不能進 git。
4. 會議錄音屬於內部資料。開發測試時，**只有本機路徑可以直接拿真實會議錄音來用**；要送 Gemini 測試，必須先得到使用者同意，或改用自己錄的測試音檔。
5. 這台 Mac 上已經有現成的 OBS 錄影，可以拿來做比對基準，見第 2 節。

---

## 1. 目標與不做的事

**目標**
- G1：錄螢幕畫面、系統聲音和麥克風，資源用量明顯比 OBS 低。
- G2：會議結束後產生繁體中文逐字稿，有時間碼。可以選本機辨識或 Gemini，兩種都要實作。
- G3：產生會議紀錄草稿（Markdown），交給既有的會議紀錄 docx 流程轉成正式紀錄。
- G4：錄到一半 App 當掉，已經錄的內容不能整段遺失。

**不做的事**
- 不做直播、場景切換、濾鏡、多來源合成（這些是 OBS 的功能）。
- 不做即時字幕（v1 只在會後處理）。
- 不做雲端同步或分享。
- v1 不直接產出 docx。

---

## 2. 現有錄影的基準（2026-11-22 實際讀取）

對象：本機既有 4 支 OBS 錄影。

| 錄影 | 長度 | 解析度 | 大小 | 平均位元率 | 編碼 |
|---|---|---|---|---|---|
| 錄影 A | 106.3 分 | 1920×1080 | 495 MB | 0.62 Mbps | H.264 + AAC 2ch 48 kHz |
| 錄影 B | 37.0 分 | 1280×720 | 120 MB | 0.43 Mbps | 同上 |
| 錄影 C | 40.6 分 | 1280×720 | 128 MB | 0.42 Mbps | 同上 |
| 錄影 D | 30.1 分 | 1280×720 | 93 MB | 0.41 Mbps | 同上 |

(verified: 用自己寫的 mov 檔頭 parser 讀 mvhd/tkhd/stsd，沒有用 ffprobe)

從這些數字可以推得：
- 位元率很低，表示畫面幾乎都是靜止的投影片或共享畫面。用每秒 30 到 60 張去擷取和編碼，大部分運算都浪費了。
- 一場會議 30 到 110 分鐘。逐字稿流程要能處理 2 小時以上的音檔。

**OBS 的資源用量還沒量過**。驗收前要在同一台 Mac、同一種畫面量一次 OBS 的基準，方法見第 9 節。

---

## 3. 整體架構

```
┌──────────────── Menu bar App（Rust）────────────────┐
│  UI：選單列圖示 ＋ 小型設定視窗                       │
│        │                                             │
│  Recorder ──► ScreenCaptureKit (SCStream)            │
│        │        ├─ 畫面 CMSampleBuffer（低張數）     │
│        │        ├─ 系統聲音                          │
│        │        └─ 麥克風（macOS 15+；舊版另接）     │
│        ▼                                             │
│  Writer ──► AVAssetWriter（VideoToolbox 硬體編碼）   │
│        │      輸出分段 MP4（fragmented）             │
│        └──► 音訊另存 16 kHz mono（給逐字稿）         │
│                                                      │
│  Transcriber（會後執行）                             │
│        ├─ Local：whisper.cpp（whisper-rs, Metal）    │
│        └─ Cloud：Gemini API（Files API）             │
│        ▼                                             │
│  PostProcess：簡轉繁、套詞彙表、合併、輸出 md/srt/json│
│        ▼                                             │
│  Minutes：會議紀錄草稿 md（Gemini，可選）            │
└──────────────────────────────────────────────────────┘
```

---

## 4. 模組設計

### 4.1 Recorder（擷取）

- 用 ScreenCaptureKit 的 `SCStream`。Rust 端可用 `screencapturekit` crate，或直接用 `objc2-screen-capture-kit`。(inferred: 未實查 crate 目前的版本、維護狀態，以及兩者涵蓋的 API)
- 擷取範圍有三種：整個螢幕、單一視窗（例如會議軟體）、指定區域。預設是使用者上次的選擇。
- 每秒張數：用 `minimumFrameInterval` 設定，**預設每秒 2 張**，可調成 1、2、5、15。ScreenCaptureKit 在畫面沒變時不會送新的一張；這點要實測，看靜止畫面時 callback 的次數。(inferred: 未實查)
- 解析度：預設 1280×720，和現有錄影一致，可選 1080p。設定 `SCStreamConfiguration` 的寬高，交給系統縮放，不要自己在 CPU 上縮。
- 游標：預設要錄，因為會議中常用游標指東西。
- 聲音：
  - 系統聲音開 `capturesAudio`（macOS 13+），並設 `excludesCurrentProcessAudio = true`。
  - 麥克風：macOS 15+ 用 SCStream 的麥克風擷取；比較舊的版本，或要指定麥克風裝置時，改用 AVAudioEngine 或 `cpal`。(inferred: 未實查 SCStream 的麥克風 API 名稱和最低版本)
  - **系統聲音和麥克風分成兩條音軌存**，之後才能分開調音量，或只辨識其中一條。
- 暫停、繼續：暫停時丟掉送進來的 buffer，繼續時要校正時間戳，不能讓錄影檔留下一段空白。

### 4.2 Writer（編碼與存檔）

- 用 AVAssetWriter，透過 `objc2-av-foundation`。視訊預設 HEVC，可選 H.264（相容性比較好）。這樣會走 VideoToolbox 硬體編碼。
- 位元率參考：720p、每秒 2 張時目標 0.3 到 0.5 Mbps。一小時約 135 到 225 MB；會比這個少多少要實測。(inferred: 未實查)
- **防當機**：
  - 設定 `movieFragmentInterval`，例如 10 秒，輸出 fragmented MP4。當機時最多只少最後一個 fragment。
  - 另外寫一個 `session.json` 記錄開始時間、設定、狀態。下次啟動時看到狀態是 `recording`，就提示使用者檢查或修復上一場錄影。
- 逐字稿用的音訊：同一份 PCM 再轉成 16 kHz 單聲道（mic 和 system 混音），另存 `audio_16k.wav`，或存 FLAC 以節省空間。轉檔用 AVAudioConverter 或 Rust 的 `rubato`。
- 磁碟空間：開始錄之前檢查剩餘空間；不到 2 GB 時警告，錄到一半不到 500 MB 時自動停止並存檔。

### 4.3 輸出資料夾結構

```
<輸出根目錄>/2026-11-25_1400_<會議名稱>/
  recording.mp4          # 視訊 + 2 條音軌
  audio_16k.flac         # 給逐字稿用
  session.json           # 設定、開始與結束時間、狀態、App 版本
  transcript.local.json  # 本機辨識原始結果（分段、時間碼）
  transcript.gemini.json # Gemini 原始結果（含講者）
  transcript.md          # 整理後的逐字稿（每段：[hh:mm:ss] 講者：內容）
  transcript.srt         # 字幕檔，可配錄影一起看
  minutes_draft.md       # 會議紀錄草稿（可選）
```

輸出根目錄預設 `~/Movies/MeetingRecorder/`，可在設定裡改。

### 4.4 Transcriber：本機路徑（預設）

- 用 `whisper-rs`（whisper.cpp 的 binding），開 Metal 加速。(inferred: 未實查目前版本與 feature flag 名稱)
- 模型：預設 `large-v3-turbo`；如果太慢，可選 `medium`。第一次使用時下載到 `~/Library/Application Support/<app>/models/`，並驗證 SHA-256。
- 參數：
  - 語言固定 `zh`。
  - 開 VAD，跳過沒人講話的段落。
  - `initial_prompt` 放專案詞彙表（見 4.6）。
- **whisper 中文常輸出簡體**，所以後處理一定要簡轉繁，用 OpenCC 的 s2twp 設定（例如 `opencc-rust` 或 `ferrous-opencc`）。(inferred: 未實查這兩個 crate 的狀態)
- 限制：本機路徑不會分辨講者。v1 就接受這個限制，講者欄位留空。
- 執行方式：背景執行、顯示進度、可以取消。辨識時不能卡住 UI，也不能和錄影搶資源，所以錄影中不准啟動本機辨識。
- 速度目標：1 小時音檔在 15 分鐘內處理完。M 系列晶片能不能達到要實測。(inferred: 未實查)

### 4.5 Transcriber：Gemini 路徑（可選）

- 用 REST API，Rust 端用 `reqwest`。沒有官方 Rust SDK 的話就自己包。
- 流程：
  1. 把 `audio_16k.flac` 切成每段約 10 分鐘，前後各重疊 15 秒。這是為了避免一次輸出太長被截斷：限制通常卡在輸出 token 上限，不在輸入長度。
  2. 每一段用 Files API 上傳，再呼叫 `generateContent`。
  3. 要求用 JSON schema 輸出：`[{start, end, speaker, text}]`，時間要加上該段的起始偏移。
  4. 合併時在重疊區去掉重複的句子。講者標籤在每段之間不保證一致，所以 prompt 要帶「前一段最後出現的講者與第一句話」幫它接上。
  5. 處理完刪除上傳的檔案（Files API 的 delete），不要等它自然過期。
- Prompt 重點：
  - 使用繁體中文（台灣），保留英文技術名詞。
  - 不要潤飾、不要摘要，照原話轉寫。
  - 聽不清楚的地方標 `[聽不清]`，不要猜。
  - 附上詞彙表。
- 模型：選當下官方文件上有的 Flash 等級音訊模型，模型名稱放設定檔，不要寫死。(inferred: 未實查 2026-11 時可用的模型名稱、單次音檔長度上限、價格)
- **資料使用條款**：
  - 必須用付費方案。免費方案的內容可能被拿去改進 Google 的產品。(inferred: 未實查目前條款，實作前要回 Google 官方的 Gemini API 條款頁確認)
  - 在設定畫面上清楚寫出「會議音檔將上傳至 Google」。每一場都要手動選 Gemini 才會上傳，**沒有預設自動上傳**。
- 錯誤處理：遇到 429 或 5xx 用指數退避重試，最多 5 次。某一段失敗時，其他段照常完成，失敗的段落標記出來，可以單獨重跑。

### 4.6 詞彙表（兩條路徑共用）

檔案：`~/Library/Application Support/<app>/glossary.txt`，一行一個詞，使用者可以自己改。初始內容示例：

```
單一登入
資料交換平台
LDAP
弱點掃描
Kubernetes
```

用法有兩種：
- 本機路徑：放進 `initial_prompt`。whisper 的 prompt 有長度上限，取前 N 個詞。
- Gemini 路徑：放進 prompt。

後處理時，可以再加一張「常見誤辨 → 正確詞」對照表（例如 `中街 → 中介`、`等地 → 等第`）做字串替換。

### 4.7 Minutes（會議紀錄草稿，可選）

- 輸入是 `transcript.md`。要啟用 Gemini 才能用，**本機路徑 v1 不做摘要**。
- 輸出 `minutes_draft.md`，章節對齊現有的例會紀錄版型：
  - 會議基本資料：日期、時間、地點、出席（由使用者在 UI 補填）
  - 討論事項：分層編號
  - 決議
  - 待辦事項表（項目／負責人／期限）
  - 下次會議前置作業
- 草稿只是草稿。每條決議和待辦都要附逐字稿的時間碼，方便人工核對；沒有時間碼可對的內容不准寫進去。
- 正式 docx 由既有的會議紀錄流程產出，不在本 App 範圍內。

### 4.8 UI

- 選單列圖示有三種狀態：待機、錄影中（紅點＋計時）、處理中（進度）。
- 選單項目：
  - 開始錄影：會議名稱可以留空，預設用時間
  - 暫停、繼續
  - 停止
  - 最近 5 場：點下去開資料夾、產生逐字稿（本機或 Gemini）、產生紀錄草稿
  - 設定
- 設定項目：
  - 擷取範圍
  - 每秒張數
  - 解析度
  - 編碼（HEVC 或 H.264）
  - 麥克風裝置
  - 輸出根目錄
  - 本機模型
  - Gemini 模型名稱與 API key（存 Keychain）
  - 詞彙表
- 全域快捷鍵可選做，例如 ⌃⌥R 開始或停止。
- 框架：
  - 只做選單列：`tray-icon` 加 `objc2-app-kit`。
  - 需要設定視窗：用 Tauri 2。
  - 由接手 agent 評估。原則是只要做得到，就選依賴比較少的那個。

---

## 5. 權限、簽章、發布

- 權限：
  - 需要「螢幕與系統錄音」和「麥克風」。`Info.plist` 要寫 `NSMicrophoneUsageDescription`。
  - 第一次使用要有引導畫面：偵測到沒有權限時，直接開啟系統設定的對應頁面。
- **簽章坑**：
  - macOS 的權限紀錄（TCC）綁在 App 的簽章和 bundle id 上。如果每次重新編譯都用 ad-hoc 簽章，權限會一直失效。
  - 開發期間要固定 bundle id，並用同一張開發者憑證簽章。沒有付費帳號時，可以用「鑰匙圈存取」自建一張程式碼簽章憑證。
- 打包：用 `cargo-bundle` 或 Tauri bundler 產生 `.app`。v1 只在本機使用，不送公證（notarization）。
- 最低系統版本：macOS 14。是否改成 15，要看麥克風擷取 API 需要哪一版，由接手 agent 查完再定。

---

## 6. 設定檔

`~/Library/Application Support/<app>/config.toml`（API key 不放這裡）：

```toml
[capture]
target = "display"        # display | window | region
fps = 2
width = 1280
height = 720
show_cursor = true
capture_system_audio = true
capture_mic = true
mic_device = "default"

[encode]
video_codec = "hevc"      # hevc | h264
video_bitrate_kbps = 400
fragment_seconds = 10

[output]
root = "~/Movies/MeetingRecorder"

[transcribe.local]
model = "large-v3-turbo"
language = "zh"
vad = true

[transcribe.gemini]
model = "<實作時依官方文件填入>"
chunk_minutes = 10
overlap_seconds = 15
```

---

## 7. 隱私與資料處理

| 資料 | 存哪裡 | 會不會離開本機 |
|---|---|---|
| 錄影、音檔 | 輸出資料夾 | 只有使用者對該場手動選 Gemini 時，才上傳 16 kHz 音檔 |
| 逐字稿、草稿 | 輸出資料夾 | 產草稿時會把逐字稿送 Gemini（要手動觸發） |
| API key | macOS Keychain | 只送 Google API |
| App log | `~/Library/Logs/<app>/` | 不會；log 裡不記逐字稿內容，也不記 API key |

- 不放任何遙測或統計上報。
- Gemini 上傳的檔案處理完立刻刪除。

---

## 8. 里程碑

| 階段 | 內容 | 完成條件 |
|---|---|---|
| M0 | 查證第 11 節的待查項目，選定 crate 與框架 | 第 11 節逐條標好結果 |
| M1 | 命令列版錄影：ScreenCaptureKit、AVAssetWriter、兩條音軌、fragmented MP4 | 錄 10 分鐘，QuickTime 能播；中途 `kill -9` 後檔案仍能播 |
| M2 | 選單列 UI、設定、暫停與繼續、磁碟空間檢查 | 完整錄一場模擬會議 |
| M3 | 本機逐字稿：whisper-rs、簡轉繁、詞彙表、md/srt | 用既有的一支 30 分鐘錄影跑完，產出繁中逐字稿 |
| M4 | Gemini 逐字稿：切段、上傳、合併、刪檔 | 用自己錄的測試音檔跑通；真實會議音檔要先問使用者 |
| M5 | 會議紀錄草稿 | 草稿每條決議都附時間碼 |
| M6 | 驗收：對照 OBS 量測 | 第 9 節指標達標 |

---

## 9. 驗收指標

在同一台 Mac 上，播放同一份投影片並切換頁面，各錄 10 分鐘，量這些數字：

| 指標 | 量法 | 目標 |
|---|---|---|
| CPU 平均 | `top -l` 或 Activity Monitor，每 5 秒取樣 | 低於 OBS 的 30% |
| 記憶體 | 同上 | 300 MB 以下 |
| 耗電 | `sudo powermetrics --samplers tasks` 看 energy impact | 低於 OBS |
| 檔案大小 | 每小時換算 | 不超過 OBS 的現值（約 185 到 280 MB/hr，由第 2 節位元率換算） |
| 畫面可讀性 | 人工看投影片文字 | 720p 下文字清楚可讀 |
| 當機保護 | 錄到一半 `kill -9` | 已錄的內容可以播，最多少 10 秒 |
| 本機逐字稿速度 | 30 分鐘音檔 | 8 分鐘內完成 |
| 逐字稿品質 | 人工抽查 3 段，每段 2 分鐘 | 專有名詞多數正確，沒有簡體字 |

OBS 的基準要用和錄現有檔案相同的 OBS 設定來量，數字記回本文件。

---

## 10. 風險

| 風險 | 影響 | 對策 |
|---|---|---|
| Rust 的 ScreenCaptureKit binding 不完整或停止維護 | M1 卡住 | 退回 `objc2-*` 直接呼叫；真的不行，擷取層改用 Swift 寫一個小 static library，由 Rust 呼叫 |
| 每秒 2 張時，畫面變化顯示得太慢 | 回放時漏看滑鼠指示 | 每秒張數可調；評估畫面有變化時暫時提高張數 |
| whisper 中文專有名詞錯很多 | 逐字稿要大量人工修正 | 詞彙表＋誤辨對照表；需要準確時改跑 Gemini |
| Gemini 各段之間講者標籤對不上 | 講者混亂 | 帶上一段的講者資訊；UI 讓使用者把「講者 A」改名後全檔替換 |
| 會議室收音差（實體會議只靠筆電麥克風） | 兩種辨識品質都差 | v1 不處理。建議外接麥克風，在 README 寫明 |
| 簽章與權限反覆失效 | 開發受阻 | 第 5 節的固定簽章做法 |
| 會議資料外送不符客戶或機關要求 | 合規問題 | 預設本機；Gemini 每場手動選擇並明確提示 |

---

## 11. 待查證清單（M0）

| # | 項目 | 結果（2026-11-22） |
|---|---|---|
| 1 | `screencapturekit` 和 `objc2-screen-capture-kit` 的最新版本、維護狀態、是否支援音訊與麥克風 | **選 `screencapturekit` 11.1.2**（2026-11-02 更新，近期下載 235 萬）。crate 內含 Swift bridge，支援系統聲音、`SCStreamOutputType::Microphone`（macOS 15）、`SCRecordingOutput`。`objc2-screen-capture-kit` 0.3.2 最後更新 2025-10-04，只是自動產生的 binding。(verified: crates.io API＋crate 原始碼) |
| 2 | SCStream 麥克風擷取的 API 名稱與最低 macOS 版本 | `SCStreamConfiguration.captureMicrophone`＋`microphoneCaptureDeviceID`，輸出型別 `.microphone`，**macOS 15.0**。因此最低系統版本定為 15。(verified: crate 原始碼 `with_captures_microphone`、feature `macos_15_0`) |
| 3 | 靜止畫面時 SCStream 是否真的不送新的一張 | **未測**：需要先授權「螢幕與系統錄音」。程式已只寫入 `SCFrameStatus::Complete` 的畫面，Idle 一律跳過。 |
| 4 | `objc2-av-foundation` 能不能完整操作 AVAssetWriter，包括 `movieFragmentInterval` | 可以：`setMovieFragmentInterval`、`setInitialMovieFragmentInterval`、`appendSampleBuffer`、`finishWritingWithCompletionHandler` 都有。(verified: 0.3.2 原始碼＋編譯通過)。`kill -9` 後能否播放**未測**。 |
| 5 | `whisper-rs` 的版本、Metal feature 名稱；`large-v3-turbo` 在本機的速度 | `whisper-rs` 0.16.0，feature `metal`，內建 Silero VAD（`ggml-silero-v6.2.0.bin`）。既有 30.1 分鐘會議錄影**71.8 秒**跑完，峰值記憶體 2.2 GB。(verified: 實測 `/usr/bin/time -l`) |
| 6 | OpenCC Rust crate 選哪一個（s2twp） | `ferrous-opencc` 0.4.0（`BuiltinConfig::S2twp`，字典內嵌）。`opencc-rust` 2.0.2 近期下載只有 1.4k，不採用。(verified: crates.io＋實測輸出為繁體) |
| 7 | Gemini 2026-11 可用的音訊模型名稱、單次音檔上限、輸出 token 上限、價格 | `gemini-3.8-flash`（stable，2026-09 更新）：輸入 1,048,576、輸出 65,536 token；單次 prompt 音訊上限 9.5 小時；音訊 32 token／秒。付費價：輸入 $0.75、輸出 $3.75／百萬 token（至 2026-12-31），2027-01-01 起 $1.50／$7.50。Files API：單檔 2 GB、48 小時自動刪除、`DELETE /v1beta/files/*`。(verified: ai.google.dev 官方模型頁、pricing、audio、files 頁；pricing 頁後半為摘要讀取) 粗估一小時音訊輸入約 11.5 萬 token ≈ US$0.09。 |
| 8 | Gemini API 付費與免費方案的資料使用條款 | 免費（Unpaid Services）：內容會用於提供、改進 Google 產品與機器學習，且可能有人工審閱。付費（綁有效 Cloud Billing 的專案）：不用 prompt 和回應改進產品，只在有限期間記錄以偵測濫用。(verified: https://ai.google.dev/gemini-api/terms，經搜尋摘要讀取) 結論：**必須用綁 billing 的專案 key**。 |
| 9 | Tauri 2 和 `tray-icon` 各自的依賴大小與 binary 大小 | 兩者都不用：選單列與設定全部用 `objc2-app-kit` 直接寫（設定放在子選單，詞彙表用文字編輯器開）。release binary **9.6 MB**，閒置 RSS 約 68 MB、CPU 0.3%。(verified: `ls`、`ps`) |
| 10 | 這台 Mac 的 macOS 版本與晶片型號 | macOS 27.2.1（26B131），Apple M5 Pro。(verified: `sw_vers`、`sysctl`) |
| 11 | OBS 在同一畫面下的 CPU、記憶體、耗電基準 | **未量**（M6）。 |

## 12. 已定案事項

| 事項 | 決定 | 日期 |
|---|---|---|
| 平台 | macOS Apple Silicon | 2026-11-22 |
| 語言 | Rust | 2026-11-22 |
| 逐字稿 | 本機（whisper）和 Gemini 都做；預設本機，Gemini 每場手動選擇 | 2026-11-22 |
| 專案位置 | 本 repo；`scripts/bundle.sh` 產出 `dist/MeetingRecorder.app` | 2026-11-22 |
| Rust 版本 | 1.100.0 stable（Rust 沒有 LTS，用最新 stable 並以 `rust-toolchain.toml` 固定），edition 2024 | 2026-11-22 |
| 最低 macOS | 15.0（麥克風擷取 API 需要） | 2026-11-22 |
| UI 框架 | `objc2-app-kit` 直接寫選單列，不用 Tauri／tray-icon | 2026-11-22 |
| 實作者 | macOS 端 agent | 2026-11-22 |

---

## 13. 實作與本文不同之處（v0.2）

| 項目 | 本文原設計 | 實作 | 理由 |
|---|---|---|---|
| 錄影容器 | fragmented MP4 | fragmented **QuickTime .mov**（`recording.mov`） | Apple 文件說 movie fragment 適用 QuickTime movie；`kill -9` 測試後若 .mp4 也可行再改 (inferred: 未實測) |
| 逐字稿音訊 | 錄影時同步轉 16 kHz 另存 FLAC | 停止後用 `AVAssetReaderAudioMixOutput` 從錄影檔混音、轉成 `audio_16k.wav` | 錄影時零額外運算；當機殘留檔和 OBS 舊錄影也能處理；30 分鐘 2.1 秒轉完 (verified) |
| 設定視窗 | 小型設定視窗 | 選單列「設定」子選單＋文字輸入對話框 | 不引入 Tauri，依賴最少 |
| Gemini 合併去重 | 比對重疊區句子 | 每段只保留「起點落在本段負責區間」的句子 | 規則簡單、可單元測試；重疊仍保留給模型當上下文 |
| 會議紀錄時間碼 | 每條附時間碼 | 另外以程式檢查，沒有時間碼的條目列在草稿最後提醒刪除 | 模型不一定守規則，用程式把關 |

## 14. 尚待驗證

- M1：實錄 10 分鐘、QuickTime 播放、`kill -9` 後可播放（需要先授權螢幕錄影）。
- 第 11 節 #3：靜止畫面的 callback 次數。
- M4／M5：實際呼叫 Gemini（需要付費專案的 API key；測試音檔用 `say` 自製，不用真實會議）。
- M6：OBS 基準與第 9 節指標。
- 固定簽章：目前是 ad-hoc 簽章，每次重編都要重新授權螢幕錄影。

# MeetingRecorder

低資源的會議錄影 App，用來取代 OBS 錄例行會議：每秒只擷取幾張畫面、用硬體編碼寫成可分段（fragmented）的影片檔，當機也只會少最後幾秒；會後用本機 whisper.cpp 或 Gemini 產生繁體中文逐字稿，並可用 Gemini 產生會議紀錄草稿。

| | macOS | Windows |
|---|---|---|
| 介面 | 選單列 | 系統匣 |
| 畫面擷取 | ScreenCaptureKit | Windows.Graphics.Capture |
| 聲音 | ScreenCaptureKit（系統聲音、麥克風） | WASAPI（loopback、麥克風） |
| 寫檔 | AVAssetWriter → fragmented `.mov` | Media Foundation SinkWriter → fragmented `.mp4` |
| 本機逐字稿 | whisper.cpp（Metal） | whisper.cpp（CPU） |
| API key | Keychain | 認證管理員 |
| 安裝檔 | `MeetingRecorder.app`（zip） | MSIX |
| 最低系統 | macOS 15 | Windows 10 2004（19041） |

設計、查證結果與尚待驗證的項目見 [`docs/DESIGN.md`](docs/DESIGN.md)。

## 下載

到 [Releases](../../releases) 下載 `MeetingRecorder-macos.zip` 或 `.msix`；每次 push 到 main 會自動發一個預先發行版（`v<版本>-build.<編號>`），推 `v*` tag 則發正式版。

## 建置

共同需求：Rust 1.100.0（`rust-toolchain.toml` 固定，rustup 會自動安裝）、CMake（編 whisper.cpp）。

**macOS**（需要 Xcode Command Line Tools）

```bash
./scripts/bundle.sh                               # dist/MeetingRecorder.app 與 dist/MeetingRecorder-macos.zip（ad-hoc 簽章）
SIGN_IDENTITY="<憑證名稱>" ./scripts/bundle.sh     # 用固定的程式碼簽章憑證，螢幕錄影權限不會因重編失效
```

**Windows**（需要 Visual Studio C++ Build Tools、Windows 10/11 SDK、LLVM）

```powershell
./scripts/package-msix.ps1 -SelfSign     # dist\MeetingRecorder_<版本>_x64.msix＋測試憑證 .cer
```

側載安裝：先把 `MeetingRecorder-test.cer` 匯入「本機電腦 → 受信任的人」，再雙擊 `.msix`。

### 上架 Microsoft Store

1. 在 Partner Center 保留 App 名稱，從「產品身分識別」頁取得 `Package/Identity/Name` 與 `Publisher`。
2. `./scripts/package-msix.ps1 -Name "<Name>" -Publisher "<Publisher>" -PublisherDisplay "<發行者顯示名稱>"`（不加 `-SelfSign`）。
3. 上傳 `.msix`，由商店簽章。App 使用 `runFullTrust`（受限功能），送審時要說明用途：桌面 App 需要擷取螢幕與系統聲音。

## 第一次使用

1. 開啟 App：macOS 在選單列出現 ⏺；Windows 在系統匣出現灰色圓點。
2. **macOS**：第一次錄影時，依提示到「系統設定 → 隱私權與安全性 → 螢幕與系統錄音」打開 MeetingRecorder 後重開 App。**Windows**：麥克風權限在「設定 → 隱私權與安全性 → 麥克風」。
3. 本機模型：`meeting-recorder models download large-v3-turbo` 與 `meeting-recorder models download silero-vad`（下載後驗證 SHA-256）。
4. Gemini（可選）：`meeting-recorder gemini set-key`。請用有綁 Cloud Billing 的專案金鑰；免費方案的內容可能被用來改進 Google 的產品。每一場都要手動選擇才會上傳。

## 命令列

`meeting-recorder help` 看完整用法：`record`、`transcribe`（`--engine local|gemini`）、`minutes`、`models`、`gemini set-key`、`extract-audio`、`sources`、`config`。Windows 安裝 MSIX 後可在終端機直接執行 `meeting-recorder`。

## 輸出

每場一個資料夾（macOS 預設 `~/Movies/MeetingRecorder/`，Windows 預設 `%USERPROFILE%\Videos\MeetingRecorder\`）：

| 檔案 | 內容 |
|---|---|
| `recording.mov`／`recording.mp4` | 視訊＋系統聲音＋麥克風（兩條音軌） |
| `session.json` | 設定、開始與結束時間、狀態 |
| `audio_16k.wav` | 逐字稿用的 16 kHz 單聲道音訊 |
| `transcript.local.json`／`transcript.gemini.json` | 辨識原始結果 |
| `transcript.md`／`transcript.srt` | 整理後的逐字稿、字幕 |
| `minutes_draft.md` | 會議紀錄草稿（每條附時間碼） |

## 收音

實體會議只靠筆電麥克風時，兩種辨識品質都會變差，建議外接麥克風。

## 授權

[MIT](LICENSE)

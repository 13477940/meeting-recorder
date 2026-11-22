# Microsoft Store 送審資料

## 產品身分識別

Partner Center →「產品管理 → 產品身分識別」。CI 已經寫在 `.github/workflows/build.yml`。

| 欄位 | 值 |
|---|---|
| `Package/Identity/Name` | `tom.li.MeetingRecorder` |
| `Package/Identity/Publisher` | `CN=907C6421-2EA7-4F0F-ACC9-A8D7649D2F0B` |
| `Package/Properties/PublisherDisplayName` | `tom.li` |

## 套件

- 到 Release 下載 `MeetingRecorder-store_<版本>_x64.msix` 和 `MeetingRecorder-store_<版本>_arm64.msix`。兩個檔案都沒有簽章，由商店簽章。
- 兩個檔案放進同一份送審一起上傳。
- 每次送審的版本號都必須比上一次高：先改 `Cargo.toml` 的 `version`，再 push 或推 tag。

## 隱私權政策網址

https://github.com/13477940/meeting-recorder/blob/main/PRIVACY.md

## 受限功能說明（runFullTrust）

貼到送審頁的「Submission options → Restricted capabilities」：

```
MeetingRecorder is a traditional Win32 desktop application (written in Rust) packaged as MSIX. It is not a UWP app, so it requires runFullTrust to start at all. Its core features rely on desktop APIs that run outside the AppContainer sandbox:

1. Screen recording of the display, window or region the user selects, via Windows.Graphics.Capture from a desktop process.
2. Recording of the system audio output (other meeting participants' voices) via WASAPI loopback capture, plus the microphone.
3. Hardware-accelerated encoding with Media Foundation into a file in the user's Videos folder.
4. Running a local speech-recognition model (whisper.cpp) to produce transcripts on the device.
5. A notification-area (system tray) icon as the main UI, and a command-line alias (meeting-recorder.exe) for advanced use.

The app does not require administrator rights. It installs no drivers or services and does not modify system settings or other applications. Recording starts only when the user explicitly starts it. All data stays on the device unless the user confirms, for each meeting, an upload to the Google Gemini API. Privacy policy: https://github.com/13477940/meeting-recorder/blob/main/PRIVACY.md
```

中文對照：本 App 是用 Rust 寫的傳統 Win32 桌面程式，以 MSIX 封裝，不是 UWP App，所以沒有 runFullTrust 就無法啟動。它的核心功能都依賴沙箱外的桌面 API：

1. 錄下使用者選擇的螢幕、視窗或範圍（Windows.Graphics.Capture）。
2. 用 WASAPI loopback 錄系統播放的聲音（其他與會者的聲音），另外錄麥克風。
3. 用 Media Foundation 硬體編碼，寫進「影片」資料夾。
4. 在本機跑語音辨識模型（whisper.cpp）產生逐字稿。
5. 主介面是系統匣圖示，另外提供命令列別名。

本 App 不需要系統管理員權限，不安裝驅動程式或服務，也不修改系統設定或其他程式。只有在使用者主動開始時才會錄影。除非使用者逐場確認上傳到 Google Gemini API，否則資料都留在本機。

## 送審前檢查

- [ ] 在乾淨的 Windows 11 實機上，x64 和 arm64 各跑過一輪：啟動、錄影、暫停、停止、本機逐字稿
- [ ] 商店截圖：系統匣選單、錄影中狀態、逐字稿輸出（從 Windows 實機截取）
- [ ] 商店說明（zh-TW）
- [ ] 年齡分級問卷（IARC）
- [ ] 價格：免費；選擇上架市場
- [ ] 版本號比上一次送審高

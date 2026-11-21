# MeetingRecorder

macOS（Apple Silicon，macOS 15 以上）選單列會議錄影 App：用 ScreenCaptureKit 錄畫面、系統聲音與麥克風，以低張數硬體編碼寫成 fragmented .mov；會後用本機 whisper.cpp 或 Gemini 產生繁體中文逐字稿，並可用 Gemini 產生會議紀錄草稿。設計與查證結果見 `docs/DESIGN.md`。

## 建置

```bash
./scripts/bundle.sh                 # 產生 dist/MeetingRecorder.app（ad-hoc 簽章）
SIGN_IDENTITY="<憑證名稱>" ./scripts/bundle.sh   # 用固定的程式碼簽章憑證，權限不會因重編失效
```

需求：Rust 1.100.0（`rust-toolchain.toml` 固定）、Xcode Command Line Tools、cmake（編 whisper.cpp）。

## 第一次使用

1. 開啟 `dist/MeetingRecorder.app`，選單列出現 ⏺。
2. 按「開始錄影…」，依提示到 系統設定 → 隱私權與安全性 → 螢幕與系統錄音 打開 MeetingRecorder，再重新開啟 App。麥克風權限第一次錄影時會詢問。
3. 本機模型：`meeting-recorder models download large-v3-turbo` 與 `silero-vad`（下載後驗證 SHA-256）。
4. Gemini（可選）：設定 →「Gemini API key（Keychain）…」。請用有綁 Cloud Billing 的專案金鑰；免費方案的內容可能被用來改進 Google 產品。

## 命令列

`dist/MeetingRecorder.app/Contents/MacOS/meeting-recorder help` 看完整用法：`record`、`transcribe`（`--engine local|gemini`）、`minutes`、`models`、`gemini set-key`、`extract-audio`、`sources`。

## 輸出

`~/Movies/MeetingRecorder/<日期_時間_會議名稱>/`：`recording.mov`（視訊＋系統聲音＋麥克風兩條音軌）、`session.json`、`audio_16k.wav`、`transcript.local.json`／`transcript.gemini.json`、`transcript.md`、`transcript.srt`、`minutes_draft.md`。

## 收音

實體會議只靠筆電麥克風時，兩種辨識品質都會變差，建議外接麥克風。

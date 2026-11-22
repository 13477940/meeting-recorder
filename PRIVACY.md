# 隱私權政策

最後更新：2026 年 11 月 22 日

本政策說明「會議錄影」（MeetingRecorder，以下稱本 App）如何處理你的資料。本 App 為開放原始碼軟體，行為可在本儲存庫的原始碼中查證。

## 摘要

- 錄影、錄音、逐字稿與會議紀錄**只存在你的電腦上**。
- 本 App **沒有帳號系統，不蒐集使用統計、當機報告或任何遙測資料**，開發者不會收到你的任何資料。
- 只有在你**每一場分別確認**後，才會把該場的音訊或逐字稿送到 Google Gemini API 處理。

## 本 App 存取的資料

| 資料 | 用途 | 儲存位置 |
|---|---|---|
| 螢幕畫面（你選擇的螢幕、視窗或範圍） | 錄製會議畫面 | 本機 |
| 系統播放的聲音 | 錄製會議中其他人的聲音 | 本機 |
| 麥克風 | 錄製你自己的聲音（可在選單關閉） | 本機 |
| 錄影檔、16 kHz 音訊、逐字稿、會議紀錄草稿、`session.json` | 會後整理 | 本機，預設在「影片」資料夾下的 `MeetingRecorder` |
| 設定、專有名詞表、校正表 | App 設定 | 本機（macOS：`~/Library/Application Support/MeetingRecorder`；Windows：`%APPDATA%\MeetingRecorder`） |
| Gemini API 金鑰（選用） | 呼叫 Gemini API | macOS 鑰匙圈／Windows 認證管理員，不寫入任何檔案 |

螢幕與麥克風權限由作業系統管理，你可以隨時在系統設定中撤銷。

## 會連線到網路的情況

本 App 只會在下列兩種情況連線，其餘時間完全離線運作：

1. **下載本機語音辨識模型**：你手動執行下載時，從 Hugging Face（`huggingface.co`）下載公開的模型檔。這只會傳送一般的 HTTP 下載請求，不含你的錄音或個人資料。
2. **使用 Gemini 產生逐字稿或會議紀錄**（選用，預設不使用）：
   - 必須先由你自行設定自己的 Gemini API 金鑰。
   - 每一場都會先跳出確認視窗，說明會送出哪些內容，經你同意才會上傳。
   - 產生逐字稿時，會把該場的音訊切段上傳到 Google Gemini API，同時附上你的專有名詞表；處理完成後，本 App 會刪除上傳的檔案（若刪除失敗，Google 也會在 48 小時後自動刪除）。
   - 產生會議紀錄時，會把該場的逐字稿文字送到 Gemini API。
   - 這些資料由 Google 依其條款處理：[Gemini API 附加服務條款](https://ai.google.dev/gemini-api/terms)、[Google 隱私權政策](https://policies.google.com/privacy)。**使用免費方案時，Google 可能會用你送出的內容改進其產品**；處理機密會議時，請使用已啟用 Cloud Billing 的付費專案金鑰，或只使用本機辨識。

## 資料分享

開發者不會取得、出售或分享你的資料。除了上述你同意送到 Google Gemini API 的內容之外，本 App 不會把資料傳給任何第三方。

## 資料保留與刪除

所有檔案都在你的電腦上，由你自行保留或刪除：刪除某場會議的資料夾，即刪除該場所有資料。要移除 API 金鑰，可在命令列執行 `meeting-recorder gemini delete-key`，或直接在 macOS 鑰匙圈／Windows 認證管理員中刪除。解除安裝本 App 不會自動刪除你的錄影資料夾。

## 兒童

本 App 不以兒童為對象，也不會刻意蒐集兒童的個人資料。

## 錄影前請取得同意

錄製會議可能涉及其他與會者的個人資料。請依你所在地的法律與組織規定，事先告知與會者並取得同意。

## 政策變更

本政策如有變更，會更新本頁並修改上方的「最後更新」日期；變更紀錄可在本儲存庫的提交歷史查詢。

## 聯絡方式

對本政策有任何問題，請在 [GitHub Issues](https://github.com/13477940/meeting-recorder/issues) 提出。

---

# Privacy Policy (English)

Last updated: November 22, 2026

- **Everything stays on your device.** Recordings, audio, transcripts and meeting minutes are stored only on your computer. The app has no accounts and collects no analytics, crash reports or telemetry; the developer receives none of your data.
- **Permissions.** Screen capture, system audio and microphone are used solely to record meetings you start. You can revoke them in your operating system settings at any time.
- **Network access** happens only in two cases:
  1. When you choose to download a speech-recognition model, the app downloads public model files from Hugging Face. No personal data is sent.
  2. Optional Google Gemini processing, off by default. It requires your own API key, stored in the macOS Keychain or Windows Credential Manager. Before each meeting is processed, the app asks for your explicit confirmation.
     - To transcribe, the meeting audio is uploaded in segments, together with your glossary. The uploaded files are deleted after processing; Google also deletes them automatically after 48 hours.
     - To generate minutes, the transcript text is sent.
     - This data is handled by Google under the [Gemini API Additional Terms](https://ai.google.dev/gemini-api/terms) and the [Google Privacy Policy](https://policies.google.com/privacy). On the free tier, Google may use submitted content to improve its products.
- **No sharing.** Apart from the content you explicitly send to Gemini, the developer does not obtain, sell or share your data.
- **Retention and deletion.** You control your files. Deleting a meeting folder deletes all of its data. Run `meeting-recorder gemini delete-key` to remove the API key. Uninstalling the app does not delete your recordings.
- **Children.** The app is not directed at children.
- **Consent.** Recording may involve other participants' personal data. Inform participants and obtain their consent as required by applicable law.
- **Changes.** Updates are posted on this page with a new date. History is available in the repository's commit log.
- **Contact.** Open an issue at [GitHub Issues](https://github.com/13477940/meeting-recorder/issues).

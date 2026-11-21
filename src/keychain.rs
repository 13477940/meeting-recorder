//! Gemini API key 只存在 macOS Keychain（service = MeetingRecorder）。

use anyhow::{Result, anyhow};

const SERVICE: &str = "MeetingRecorder";
const ACCOUNT: &str = "gemini-api-key";

pub fn gemini_key() -> Result<String> {
    let v = security_framework::passwords::get_generic_password(SERVICE, ACCOUNT)
        .map_err(|_| anyhow!("Keychain 找不到 Gemini API key，請先執行：meeting-recorder gemini set-key"))?;
    let k = String::from_utf8(v).map_err(|_| anyhow!("Keychain 裡的 key 不是文字"))?;
    Ok(k.trim().to_string())
}

pub fn set_gemini_key(key: &str) -> Result<()> {
    security_framework::passwords::set_generic_password(SERVICE, ACCOUNT, key.trim().as_bytes())
        .map_err(|e| anyhow!("寫入 Keychain 失敗：{e}"))
}

pub fn delete_gemini_key() -> Result<()> {
    security_framework::passwords::delete_generic_password(SERVICE, ACCOUNT)
        .map_err(|e| anyhow!("刪除失敗：{e}"))
}

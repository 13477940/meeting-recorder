//! Gemini API key 只存在 Windows 認證管理員（一般認證，目標名稱 MeetingRecorder:gemini-api-key）。

use anyhow::{Result, anyhow};
use windows::Win32::Security::Credentials::{
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW, CredWriteW,
};
use windows::core::{PCWSTR, PWSTR};

const TARGET: &str = "MeetingRecorder:gemini-api-key";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn gemini_key() -> Result<String> {
    let t = wide(TARGET);
    let mut p: *mut CREDENTIALW = std::ptr::null_mut();
    unsafe { CredReadW(PCWSTR(t.as_ptr()), CRED_TYPE_GENERIC, None, &mut p) }
        .map_err(|_| anyhow!("認證管理員找不到 Gemini API key，請先執行：meeting-recorder gemini set-key"))?;
    let key = unsafe {
        let c = &*p;
        let blob = std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize);
        let k = String::from_utf8(blob.to_vec());
        CredFree(p as *const _);
        k
    };
    Ok(key.map_err(|_| anyhow!("認證管理員裡的 key 不是文字"))?.trim().to_string())
}

pub fn set_gemini_key(key: &str) -> Result<()> {
    let mut t = wide(TARGET);
    let mut user = wide("gemini-api-key");
    let mut blob = key.trim().as_bytes().to_vec();
    let cred = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(t.as_mut_ptr()),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: PWSTR(user.as_mut_ptr()),
        ..Default::default()
    };
    unsafe { CredWriteW(&cred, 0) }.map_err(|e| anyhow!("寫入認證管理員失敗：{e}"))
}

pub fn delete_gemini_key() -> Result<()> {
    let t = wide(TARGET);
    unsafe { CredDeleteW(PCWSTR(t.as_ptr()), CRED_TYPE_GENERIC, None) }.map_err(|e| anyhow!("刪除失敗：{e}"))
}

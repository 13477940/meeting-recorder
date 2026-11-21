//! 本機辨識模型：下載到 ~/Library/Application Support/MeetingRecorder/models/ 並驗證 SHA-256。

use std::io::{Read, Write};

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};

use crate::local_asr::{MODELS, model, models_dir};

pub fn list() -> String {
    let mut s = String::new();
    for m in MODELS {
        let p = models_dir().join(m.file);
        let st = if p.exists() { "已下載" } else { "未下載" };
        s.push_str(&format!("{:16} {:>8.1} MB  {st}  {}\n", m.name, m.size as f64 / 1e6, p.display()));
    }
    s
}

pub fn sha256_file(p: &std::path::Path) -> Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

pub fn download(name: &str) -> Result<()> {
    let m = model(name)?;
    std::fs::create_dir_all(models_dir())?;
    let dest = models_dir().join(m.file);
    if dest.exists() {
        eprintln!("已存在，驗證中…");
        if sha256_file(&dest)? == m.sha256 {
            eprintln!("SHA-256 相符：{}", dest.display());
            return Ok(());
        }
        bail!("{} 的 SHA-256 不符，請刪除後重新下載", dest.display());
    }
    let part = dest.with_extension("part");
    let mut resp = reqwest::blocking::Client::builder().timeout(None).build()?.get(m.url).send()?.error_for_status()?;
    let mut f = std::fs::File::create(&part)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut got: u64 = 0;
    loop {
        let n = resp.read(&mut buf)?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n])?;
        h.update(&buf[..n]);
        got += n as u64;
        eprint!("\r下載 {name}：{:.0}%", got as f64 * 100.0 / m.size as f64);
    }
    eprintln!();
    f.sync_all()?;
    let digest: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if digest != m.sha256 {
        let _ = std::fs::remove_file(&part);
        bail!("SHA-256 不符（預期 {}，實得 {digest}），已刪除下載檔", m.sha256);
    }
    std::fs::rename(&part, &dest)?;
    eprintln!("完成並驗證：{}", dest.display());
    Ok(())
}

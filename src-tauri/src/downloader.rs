use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MAX_RETRIES: u32 = 5;
const BACKOFF_BASE_MS: u64 = 500;
const CHUNK: usize = 64 * 1024;

pub struct Downloader {
    client: reqwest::blocking::Client,
}

impl Default for Downloader {
    fn default() -> Self {
        Self::new()
    }
}

/// Whisper.cpp model files start with the GGML v3 magic: a little-endian
/// uint32 0x67676d6c ("ggml"), which appears on disk as the bytes "lmgg".
const GGML_MAGIC: &[u8; 4] = b"lmgg";

/// Whisper.cpp model files start with the "ggml" magic bytes.
pub fn is_valid_model_file(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).is_ok() && &magic == GGML_MAGIC
}

/// Generic validity: exists and (optionally) matches the expected byte size.
/// Used for ONNX bundles and JSON sidecar files where there is no magic.
pub fn is_valid_download(path: &Path, expected_size: u64) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if expected_size > 0 {
        return meta.len() == expected_size;
    }
    meta.len() > 0
}

impl Downloader {
    pub fn new() -> Self {
        let client = reqwest::blocking::Client::builder()
            .user_agent("VoiceKeyboard/0.1")
            .connect_timeout(Duration::from_secs(15))
            .build()
            .expect("failed to build http client");
        Self { client }
    }

    pub fn is_downloaded(&self, dest: &Path, expected_sha256: &str) -> bool {
        if !dest.exists() {
            return false;
        }
        if !expected_sha256.is_empty() {
            return sha256_of(dest)
                .map(|sum| sum.eq_ignore_ascii_case(expected_sha256))
                .unwrap_or(false);
        }
        is_valid_model_file(dest)
    }

    pub fn download<F>(&self, url: &str, dest: &Path, expected_sha256: &str, mut progress: F) -> Result<(), String>
    where
        F: FnMut(u64, u64),
    {
        let parent = dest.parent().ok_or("invalid destination path")?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;

        if self.is_downloaded(dest, expected_sha256) {
            progress(1, 1);
            return Ok(());
        }

        let part: PathBuf = {
            let mut p = dest.as_os_str().to_os_string();
            p.push(".part");
            p.into()
        };

        for attempt in 0..=MAX_RETRIES {
            match self.try_download(url, &part, &mut progress) {
                Ok(()) => break,
                Err(e) => {
                    if attempt == MAX_RETRIES {
                        return Err(format!("download failed after {} attempts: {e}", MAX_RETRIES + 1));
                    }
                    let delay = BACKOFF_BASE_MS * 2u64.pow(attempt);
                    std::thread::sleep(Duration::from_millis(delay));
                }
            }
        }

        if !expected_sha256.is_empty() {
            let actual = sha256_of(&part).map_err(|e| e.to_string())?;
            if !actual.eq_ignore_ascii_case(expected_sha256) {
                let _ = std::fs::remove_file(&part);
                return Err(format!("checksum mismatch: expected {expected_sha256}, got {actual}"));
            }
        } else if !is_valid_model_file(&part) {
            let _ = std::fs::remove_file(&part);
            return Err("downloaded file is invalid (not a Whisper model)".to_string());
        }

        std::fs::rename(&part, dest).map_err(|e| e.to_string())?;
        progress(1, 1);
        Ok(())
    }

    /// Download a single file without whisper-specific validation (used for
    /// ONNX bundle members and sidecars). Resumes partial downloads.
    pub fn download_raw<F>(&self, url: &str, dest: &Path, mut progress: F) -> Result<(), String>
    where
        F: FnMut(u64, u64),
    {
        let parent = dest.parent().ok_or("invalid destination path")?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;

        if is_valid_download(dest, 0) {
            progress(1, 1);
            return Ok(());
        }

        let part: PathBuf = {
            let mut p = dest.as_os_str().to_os_string();
            p.push(".part");
            p.into()
        };

        for attempt in 0..=MAX_RETRIES {
            match self.try_download(url, &part, &mut progress) {
                Ok(()) => break,
                Err(e) => {
                    if attempt == MAX_RETRIES {
                        return Err(format!("download failed after {} attempts: {e}", MAX_RETRIES + 1));
                    }
                    let delay = BACKOFF_BASE_MS * 2u64.pow(attempt);
                    std::thread::sleep(Duration::from_millis(delay));
                }
            }
        }

        std::fs::rename(&part, dest).map_err(|e| e.to_string())?;
        progress(1, 1);
        Ok(())
    }

    /// Download a multi-file bundle (ONNX models etc.) into `dir`, reporting
    /// cumulative progress across all files. Skips files already present with
    /// the expected size. Returns the list of absolute paths in order.
    pub fn download_bundle<F>(
        &self,
        dir: &Path,
        files: &[(String, String, u64)],
        mut progress: F,
    ) -> Result<Vec<std::path::PathBuf>, String>
    where
        F: FnMut(u64, u64),
    {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let total: u64 = files.iter().map(|(_, _, size)| *size).sum();
        let mut done = 0u64;
        let mut out = Vec::with_capacity(files.len());

        for (filename, url, size) in files {
            let dest = dir.join(filename);
            if !is_valid_download(&dest, *size) {
                let d = done;
                let cell = std::cell::RefCell::new(&mut progress);
                self.download_raw(url, &dest, move |file_done, file_total| {
                    let merged = d + if file_total > 0 { file_done } else { 0 };
                    cell.borrow_mut()(merged.min(total), total);
                })
                .map_err(|e| format!("{filename}: {e}"))?;
            }
            done += *size;
            out.push(dest);
            progress(done.min(total), total);
        }
        Ok(out)
    }

    fn try_download<F>(&self, url: &str, part: &Path, progress: &mut F) -> Result<(), String>
    where
        F: FnMut(u64, u64),
    {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(part)
            .map_err(|e| e.to_string())?;

        let start = file.metadata().map_err(|e| e.to_string())?.len();

        let mut request = self.client.get(url);
        let mut started_at_zero = start == 0;
        if start > 0 {
            request = request.header("Range", format!("bytes={start}-"));
        }
        let mut response = request.send().map_err(|e| e.to_string())?;

        let status = response.status();
        if !status.is_success() {
            return Err(format!("HTTP {status} while downloading {url}"));
        }

        if start > 0 && status != reqwest::StatusCode::PARTIAL_CONTENT {
            file.set_len(0).map_err(|e| e.to_string())?;
            file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
            started_at_zero = true;
            response = self.client.get(url).send().map_err(|e| e.to_string())?;
        }

        let total = response.content_length().map(|l| l + if started_at_zero { 0 } else { start }).unwrap_or(0);
        let mut downloaded = if started_at_zero { 0 } else { start };
        let mut buffer = vec![0u8; CHUNK];
        loop {
            let n = response.read(&mut buffer).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            file.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
            downloaded += n as u64;
            if total > 0 {
                progress(downloaded, total);
            }
        }
        file.flush().map_err(|e| e.to_string())?;
        if total > 0 && downloaded < total {
            return Err("connection closed before download completed".to_string());
        }
        Ok(())
    }
}

pub fn sha256_of(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn sha256_known_vector() {
        let dir = std::env::temp_dir().join(format!("vk-sha-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.txt");
        let mut f = File::create(&path).unwrap();
        f.write_all(b"hello world\n").unwrap();
        assert_eq!(sha256_of(&path).unwrap(), "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn valid_model_magic() {
        let dir = std::env::temp_dir().join(format!("vk-magic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.bin");
        let bad = dir.join("bad.bin");
        std::fs::write(&good, b"lmgg...").unwrap();
        std::fs::write(&bad, b"Entry not found").unwrap();
        assert!(is_valid_model_file(&good));
        assert!(!is_valid_model_file(&bad));
        assert!(!is_valid_model_file(&dir.join("missing.bin")));
        std::fs::remove_dir_all(&dir).ok();
    }
}

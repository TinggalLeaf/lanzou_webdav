//! 高级上传助手 [`Uploader`]。
//!
//! 该模块在 [`crate::client::LanzouCloud`] 之上提供一个"一次性"上传入口：
//!
//! - 计算 MD5
//! - 根据大小自动选择单文件 / 分片目录上传
//! - 通过回调驱动进度
//! - 返回结构化 [`UploadResult`]
//!
//! 调用方一般使用 [`Uploader::upload_path`] 或 [`Uploader::upload_bytes`] 即可。

use crate::client::{LanzouCloud, CHUNK_LIMIT};
use crate::error::{LanzouError, Result};
use crate::vfs::safe_lanzou_ext;
use bytes::Bytes;
use md5::Context;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// 上传进度回调类型。
///
/// 参数：(已上传字节, 总字节)。
pub type ProgressCb = Arc<dyn Fn(usize, usize) + Send + Sync>;

/// 上传结果。
#[derive(Debug, Clone)]
pub struct UploadResult {
    /// 蓝奏云文件 ID（单文件）或分片目录 ID（分片）
    pub lanzou_id: String,
    /// 文件 MD5（小写 32 位 hex）
    pub md5: String,
    /// 分片数（1 表示不分片）
    pub chunks: u32,
    /// 文件总字节
    pub size: usize,
}

/// 上传助手（轻量包装）。
pub struct Uploader<'a> {
    client: &'a mut LanzouCloud,
}

impl<'a> Uploader<'a> {
    /// 创建一个绑定到现有客户端的上传助手。
    pub fn new(client: &'a mut LanzouCloud) -> Self {
        Self { client }
    }

    /// 上传本地文件。
    ///
    /// # 参数
    /// - `path`：本地文件路径
    /// - `target_folder`：目标蓝奏云文件夹 ID（`-1` 表示根目录）
    /// - `progress`：可选进度回调
    pub async fn upload_path(
        &mut self,
        path: &Path,
        target_folder: &str,
        progress: Option<ProgressCb>,
    ) -> Result<UploadResult> {
        let bytes = tokio::fs::read(path).await?;
        let original_name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .ok_or_else(|| LanzouError::Invalid("无法获取文件名".into()))?;
        self.upload_bytes(&bytes, &original_name, target_folder, progress)
            .await
    }

    /// 上传字节流。
    ///
    /// 自动选择单文件或分片上传（基于 [`CHUNK_LIMIT`]）。
    pub async fn upload_bytes(
        &mut self,
        bytes: &[u8],
        original_name: &str,
        target_folder: &str,
        progress: Option<ProgressCb>,
    ) -> Result<UploadResult> {
        let total = bytes.len();
        let mut hasher = Context::new();
        hasher.consume(bytes);
        let md5_hex = format!("{:x}", hasher.compute());

        let ext = Path::new(original_name)
            .extension()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let safe_ext = safe_lanzou_ext(&ext);

        let noop: ProgressCb = Arc::new(|_, _| {});
        let progress = progress.unwrap_or(noop);

        if total <= CHUNK_LIMIT {
            let safe_name = format!("{md5_hex}.{safe_ext}");
            let chunk = Bytes::copy_from_slice(bytes);
            let id = self
                .client
                .upload_small(chunk, &safe_name, target_folder, progress.clone())
                .await?;
            progress(total, total);
            return Ok(UploadResult {
                lanzou_id: id,
                md5: md5_hex,
                chunks: 1,
                size: total,
            });
        }

        // 分片：以 MD5 作分片目录名（蓝奏云文件夹名仅允许字母数字下划线）
        let safe_md5: String = md5_hex
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let chunk_folder_id = self
            .ensure_chunk_folder(&safe_md5, target_folder)
            .await?;

        let chunk_count = ((total + CHUNK_LIMIT - 1) / CHUNK_LIMIT) as u32;
        let loaded = Arc::new(AtomicUsize::new(0));
        for i in 0..chunk_count {
            let start = (i as usize) * CHUNK_LIMIT;
            let end = std::cmp::min(start + CHUNK_LIMIT, total);
            let chunk = Bytes::copy_from_slice(&bytes[start..end]);
            let chunk_name = crate::crypto::encrypt_chunk_filename(&md5_hex, i + 1);
            let loaded = loaded.clone();
            let progress = progress.clone();
            let id = self
                .client
                .upload_small(
                    chunk,
                    &chunk_name,
                    &chunk_folder_id,
                    Arc::new(move |cur, _chunk_total| {
                        let cur = cur.min(end - start);
                        let prev = loaded.fetch_add(cur, Ordering::SeqCst);
                        let total_now = std::cmp::min(prev + cur, total);
                        progress(total_now, total);
                    }),
                )
                .await?;
            if id.is_empty() {
                return Err(LanzouError::Api(format!(
                    "分片 {}/{} 返回了空 ID",
                    i + 1,
                    chunk_count
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        progress(total, total);
        Ok(UploadResult {
            lanzou_id: chunk_folder_id,
            md5: md5_hex,
            chunks: chunk_count,
            size: total,
        })
    }

    /// 在 `target_folder` 下查找或创建名为 `safe_md5` 的分片目录。
    async fn ensure_chunk_folder(
        &mut self,
        safe_md5: &str,
        target_folder: &str,
    ) -> Result<String> {
        let parent_id = if target_folder == "-1" { "0" } else { target_folder };
        let saved = self.client.current_folder();
        self.client.enter_folder(parent_id);
        let folders = self.client.list_folders().await?;
        for f in folders {
            let name = f["name_all"]
                .as_str()
                .or_else(|| f["name"].as_str())
                .unwrap_or("");
            if name == safe_md5 {
                if let Some(s) = f["fol_id"].as_str() {
                    self.client.go_back();
                    return Ok(s.to_string());
                }
                if let Some(n) = f["fol_id"].as_u64() {
                    self.client.go_back();
                    return Ok(n.to_string());
                }
            }
        }
        let res = self
            .client
            .create_folder_in_target(safe_md5, "", parent_id)
            .await?;
        self.client.go_back();
        let _ = saved;
        if let Some(s) = res["text"].as_str() {
            Ok(s.to_string())
        } else if let Some(id) = res["text"]["id"].as_str() {
            Ok(id.to_string())
        } else {
            Err(LanzouError::Api("分片目录创建返回 ID 为空".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn safe_ext_for_iso_override() {
        assert_eq!(safe_lanzou_ext("docx"), "docx");
        assert_eq!(safe_lanzou_ext("unknown"), "iso");
    }

    #[test]
    fn write_then_read() {
        let mut p = std::env::temp_dir();
        p.push("lanzou_uploader_test.bin");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(b"hello world").unwrap();
        drop(f);
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(bytes, b"hello world");
        let _ = std::fs::remove_file(&p);
    }
}
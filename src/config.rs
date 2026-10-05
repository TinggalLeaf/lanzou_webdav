//! 配置文件加载与持久化。
//!
//! 配置文件路径：`data/config.toml`。
//!
//! 配置项示例：
//! ```toml
//! # 监听端口
//! listen_port = 8080
//!
//! [webdav]
//! # WebDAV Basic Auth 凭证
//! username = "admin"
//! password = "admin"
//!
//! [lanzou]
//! # 蓝奏云账号（手机号或用户名）
//! username = "13800000000"
//! # 蓝奏云密码（登录成功后会清空）
//! password = "your-password"
//! ```
//!
//! 第一次启动时若配置文件不存在，将自动生成一份默认配置（仍需用户手动
//! 填入蓝奏云账号密码后重启）。

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;

/// 顶层配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// HTTP 监听端口
    pub listen_port: u16,
    /// WebDAV Basic Auth 凭证
    pub webdav: WebDavAuth,
    /// 蓝奏云账号（用于首次自动登录）
    pub lanzou: LanzouAuth,
}

/// WebDAV 鉴权配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebDavAuth {
    /// WebDAV 用户名
    pub username: String,
    /// WebDAV 密码
    pub password: String,
}

/// 蓝奏云账号配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanzouAuth {
    /// 手机号 / 用户名
    pub username: String,
    /// 密码（登录成功后将自动清空，避免明文长期落盘）
    pub password: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_port: 8080,
            webdav: WebDavAuth {
                username: "admin".into(),
                password: "admin".into(),
            },
            lanzou: LanzouAuth {
                username: String::new(),
                password: String::new(),
            },
        }
    }
}

impl Config {
    /// 加载 `path`，不存在则生成默认值。
    pub async fn load_or_init(path: &Path) -> Result<Self> {
        if path.exists() {
            let data = fs::read_to_string(path)
                .await
                .with_context(|| format!("读取 {} 失败", path.display()))?;
            let cfg: Config = toml::from_str(&data)
                .with_context(|| format!("解析 {} 失败，请检查 TOML 语法", path.display()))?;
            Ok(cfg)
        } else {
            warn_user_no_config(path);
            Ok(Config::default())
        }
    }

    /// 把当前配置写回磁盘。
    pub async fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await.ok();
        }
        let s = toml::to_string_pretty(self).map_err(|e| anyhow!("序列化配置失败: {e}"))?;
        fs::write(path, s)
            .await
            .with_context(|| format!("写入 {} 失败", path.display()))?;
        Ok(())
    }

    /// 数据目录路径（保留以便外部调用）。
    #[allow(dead_code)]
    pub fn data_dir_for(path: &Path) -> PathBuf {
        path.parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

fn warn_user_no_config(path: &Path) {
    tracing::warn!(
        "未找到配置文件 {}，已使用内存默认值；首次登录后会自动写入",
        path.display()
    );
}
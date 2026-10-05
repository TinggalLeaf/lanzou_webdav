//! `lanzou_webdav` —— 把蓝奏云挂载成可被 Infuse / 极影视 / RaiDrive 等读取的 WebDAV。
//!
//! ## 设计原则
//!
//! - **配置可外部化**：`config.toml` 必须位于 `data/` 目录，方便 Docker volume 挂载
//! - **账号持久化**：登录后写到 `data/account.json`，重启服务无需重新登录
//! - **最小依赖**：仅依赖 `axum` + `lanzou-sdk`，无 Tauri / 数据库
//! - **Docker 友好**：单一可执行文件 + data 目录即可运行
//!
//! ## 启动流程
//!
//! 1. 读取 `<DATA_DIR>/config.toml`，加载监听端口、WebDAV 账号密码
//! 2. 尝试读取 `<DATA_DIR>/account.json`，恢复已登录的蓝奏云会话
//! 3. 若会话失效或不存在，使用 `config.toml` 中的蓝奏云账号登录
//! 4. 确保 `.heriheri` / `.deeperdir` 两个根目录存在
//! 5. 加载 / 初始化 VFS 树
//! 6. 绑定监听端口并启动 axum HTTP 服务

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info};

mod config;
mod state;
mod webdav;

use crate::config::Config;
use crate::state::AppState;

/// 解析数据目录路径：
///   1. 优先使用环境变量 `LANZOU_DATA_DIR`
///   2. 否则使用 `DATA_DIR` 环境变量（兼容 HeriHeri 风格）
///   3. 否则使用 `./data`（相对当前工作目录）
fn resolve_data_dir() -> PathBuf {
    std::env::var("LANZOU_DATA_DIR")
        .or_else(|_| std::env::var("DATA_DIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./data"))
}

/// tokio 主入口。
#[tokio::main]
async fn main() -> Result<()> {
    // 初始化 tracing，遵循 RUST_LOG；默认 info 级别
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,lanzou_sdk=info".into()),
        )
        .with_target(false)
        .compact()
        .init();

    let data_dir = resolve_data_dir();
    if !data_dir.exists() {
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("无法创建数据目录 {}", data_dir.display()))?;
    }
    info!("📂 数据目录: {}", data_dir.display());

    // 加载 / 创建 config.toml
    let config_path = data_dir.join("config.toml");
    let config = Config::load_or_init(&config_path).await?;
    config.save(&config_path).await?;
    info!(
        "⚙️  配置加载完成：监听 0.0.0.0:{}，WebDAV 用户 '{}'",
        config.listen_port, config.webdav.username
    );

    // 构造共享状态
    let state = AppState::initialize(data_dir.clone(), &config)
        .await
        .context("初始化应用状态失败")?;

    // 启动 WebDAV HTTP 服务
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.listen_port))
        .await
        .with_context(|| format!("无法绑定 0.0.0.0:{}", config.listen_port))?;
    let bound_port = listener.local_addr()?.port();
    info!("🚀 WebDAV 服务已启动：0.0.0.0:{bound_port}/dav");

    let shared = Arc::new(state);
    if let Err(err) = webdav::serve(listener, shared.clone()).await {
        error!("WebDAV 服务异常退出：{err:?}");
        return Err(err);
    }
    Ok(())
}
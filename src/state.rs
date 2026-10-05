//! WebDAV 服务全局共享状态。
//!
//! `AppState` 持有蓝奏云客户端、VFS 树、目录栈等需要在 HTTP 请求间共享的状态。
//! `Mutex` 包裹可保证 axum 的多任务并发安全。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use lanzou_sdk::{account, LanzouCloud, LanzouDownloader, VfsTree};
use tracing::{info, warn};

use crate::config::Config;

/// WebDAV 服务的全局状态。
pub struct AppState {
    /// 已登录的蓝奏云客户端（PUT/MKCOL/PROPFIND 等需要登录的操作）
    pub client: Arc<tokio::sync::Mutex<LanzouCloud>>,
    /// 公开分享直链解析器（GET 流式代理无需登录）
    pub downloader: Arc<LanzouDownloader>,
    /// VFS 树
    pub vfs: Arc<tokio::sync::Mutex<Option<VfsTree>>>,
    /// WebDAV Basic Auth 凭证（启动时确定，不在运行时变更）
    pub webdav_user: String,
    pub webdav_pass: String,
    /// 数据目录（保留以便调试 / 未来扩展）
    #[allow(dead_code)]
    pub data_dir: PathBuf,
    /// 蓝奏云账号持久化路径（保留以便调试 / 未来扩展）
    #[allow(dead_code)]
    pub account_path: PathBuf,
    /// 用户当前所在目录栈（保留以便未来手动浏览）
    #[allow(dead_code)]
    pub pid_stack: Arc<tokio::sync::Mutex<Vec<u64>>>,
}

impl AppState {
    /// 初始化全局状态：尝试加载已有会话 / 配置账号登录 / 初始化 VFS 根。
    pub async fn initialize(data_dir: PathBuf, config: &Config) -> Result<Self> {
        let account_path = data_dir.join("account.json");
        let tree_path = data_dir.join("vfs_tree.tsv");

        // 1. 构造客户端
        let mut client = LanzouCloud::new();

        // 2. 优先尝试加载已持久化的会话
        let mut logged_in = false;
        if let Ok(session) = account::load_session(&account_path) {
            if session.is_valid() {
                info!(
                    "🔐 已加载持久化的蓝奏云会话：账号 {}",
                    session.account
                );
                client.set_session(&session);
                if client.is_logged_in().await {
                    logged_in = true;
                } else {
                    warn!("⚠️ 持久化的蓝奏云会话已失效，将重新登录");
                }
            }
        }

        // 3. 若仍未登录，则根据 config 自动登录
        if !logged_in && !config.lanzou.username.is_empty() {
            info!(
                "🔑 正在使用账号 {} 登录蓝奏云…",
                config.lanzou.username
            );
            match client
                .login(&config.lanzou.username, &config.lanzou.password)
                .await
            {
                Ok(session) => {
                    account::save_session(&account_path, &session)?;
                    info!(
                        "✅ 蓝奏云登录成功，会话已写入 {}",
                        account_path.display()
                    );
                    logged_in = true;
                }
                Err(e) => {
                    warn!("⚠️ 蓝奏云登录失败：{e}");
                }
            }
        }

        let mut vfs_tree: Option<VfsTree> = None;
        if logged_in {
            // 4. 确保 VFS 根目录存在
            match client.ensure_vfs_roots().await {
                Ok((root_id, deeper_id)) => {
                    info!(
                        "📁 VFS 根目录就绪：.heriheri={} / .deeperdir={}",
                        root_id, deeper_id
                    );
                    let tree = match VfsTree::load_local(tree_path.clone()) {
                        Ok(mut t) => {
                            if t.deeperdir_lanzou_id.is_empty() {
                                t.deeperdir_lanzou_id = deeper_id.clone();
                                let _ = t.save_local();
                            }
                            t
                        }
                        Err(_) => {
                            let t = VfsTree::new(root_id, deeper_id, tree_path.clone());
                            let _ = t.save_local();
                            t
                        }
                    };
                    vfs_tree = Some(tree);
                }
                Err(e) => {
                    warn!("⚠️ 无法创建 VFS 根目录（{e}），WebDAV 将无法列出文件");
                }
            }
        } else {
            warn!(
                "⚠️ 当前未登录蓝奏云，请在 {} 中填写 lanzou.username/password 后重启",
                account_path.display()
            );
        }

        Ok(Self {
            client: Arc::new(tokio::sync::Mutex::new(client)),
            downloader: Arc::new(LanzouDownloader::new()),
            vfs: Arc::new(tokio::sync::Mutex::new(vfs_tree)),
            webdav_user: config.webdav.username.clone(),
            webdav_pass: config.webdav.password.clone(),
            data_dir,
            account_path,
            pid_stack: Arc::new(tokio::sync::Mutex::new(vec![0])),
        })
    }
}
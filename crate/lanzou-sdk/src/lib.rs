//! # lanzou-sdk
//!
//! 蓝奏云 (Lanzou Cloud / woozooo) 非官方 Rust SDK。
//!
//! 本 crate 提供蓝奏云的完整封装，**不依赖任何浏览器或 JS 引擎**，所有 WAF 校验、
//! 加密混淆均使用纯 Rust 实现。
//!
//! ## 功能模块
//!
//! - [`client`] —— 已登录账号的 [`LanzouCloud`] 客户端，提供文件/文件夹的增删改查、上传等能力
//! - [`downloader`] —— 公开分享链接的直链解析（无需登录），适用于 WebDAV 流媒体代理
//! - [`crypto`] —— 阿里 WAF (acw_sc__v2) 校验算法与分片文件名加密/解密工具
//! - [`vfs`] —— 蓝奏云之上的虚拟文件系统模型，支持去重与回收站
//! - [`uploader`] —— 分片上传、断点续传、看门狗自动重试
//! - [`account`] —— 会话 Cookie 持久化与手机号注册
//! - [`error`] —— 统一错误类型 [`LanzouError`]
//!
//! ## 快速开始
//!
//! ```no_run
//! use lanzou_sdk::{LanzouCloud, Result};
//!
//! # async fn run() -> Result<()> {
//! let mut client = LanzouCloud::new();
//! // 登录
//! client.login("13800000000", "password").await?;
//! // 列出根目录文件夹
//! let folders = client.list_folders().await?;
//! println!("{:#?}", folders);
//! # Ok(()) }
//! ```

#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod account;
pub mod client;
pub mod crypto;
pub mod downloader;
pub mod error;
pub mod model;
pub mod uploader;
pub mod vfs;

// 重新导出最常用的类型，方便上层使用
pub use client::LanzouCloud;
pub use downloader::LanzouDownloader;
pub use error::{LanzouError, Result};
pub use model::{AccountCredential, LanzouFile, LanzouFolder, NodeType, Session, ShareInfo, SharePayload};
pub use vfs::{VfsNode, VfsTree};
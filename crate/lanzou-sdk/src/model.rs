//! 蓝奏云数据模型与共享类型定义。

use serde::{Deserialize, Serialize};

/// 节点类型。
///
/// 蓝奏云本身不区分文件/文件夹（统一为 `fol_id`），但在本 SDK 中我们通过
/// 列出文件列表时返回的字段来推断 `File` 或 `Directory`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum NodeType {
    /// 文件夹
    D,
    /// 文件
    F,
}

impl NodeType {
    /// 从单字符标记解析，解析失败默认按文件处理。
    pub fn from_mark(mark: &str) -> Self {
        match mark {
            "D" | "d" => NodeType::D,
            _ => NodeType::F,
        }
    }

    /// 序列化为单字符标记。
    pub fn as_mark(&self) -> &'static str {
        match self {
            NodeType::D => "D",
            NodeType::F => "F",
        }
    }
}

/// 登录账号信息（含密码），用于通过配置文件自动登录。
///
/// 该结构体通常从 `config.toml` 解析而来，**密码字段不要长期明文落盘**，
/// 仅作为首次自动登录使用，登录成功后立刻转为 [`Session`] 存到
/// `account.json`，同时把 `config.toml` 的密码字段置空。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountCredential {
    /// 账号（手机号或用户名）
    pub username: String,
    /// 密码
    pub password: String,
}

/// 已登录会话信息。
///
/// 登录成功后 `ylogin` 与 `phpdisk_info` 这两个 Cookie 是后续所有已登录
/// 请求的凭证，需要持久化到本地。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Session {
    /// `ylogin` Cookie 值
    pub ylogin: String,
    /// `phpdisk_info` Cookie 值
    pub phpdisk_info: String,
    /// 登录账号（手机号）
    #[serde(default)]
    pub account: String,
}

impl Session {
    /// 是否拥有合法的 Cookie。
    pub fn is_valid(&self) -> bool {
        !self.ylogin.is_empty() && !self.phpdisk_info.is_empty()
    }
}

/// 文件信息（来自 list_files）。
///
/// 注意：蓝奏云接口对字段命名不一致，此处用 `serde_json::Value` 兼容，
/// 同时提供常用字段的便捷访问方法。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanzouFile {
    /// 文件 ID
    pub id: String,
    /// 文件名（不带扩展名）
    pub name: String,
    /// 完整文件名（含扩展名）
    #[serde(default)]
    pub name_all: String,
    /// 大小（人类可读，如 "1.2 MB"）
    #[serde(default)]
    pub size: String,
    /// 文件夹 ID
    #[serde(default)]
    pub fol_id: Option<String>,
    /// 时间戳（秒）
    #[serde(default)]
    pub time: Option<String>,
    /// 文件 MD5（部分接口返回）
    #[serde(default)]
    pub md5: Option<String>,
}

/// 文件夹信息（来自 list_folders）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanzouFolder {
    /// 文件夹 ID
    pub fol_id: String,
    /// 文件夹名
    pub name: String,
    /// 完整文件夹名
    #[serde(default)]
    pub name_all: String,
    /// 文件夹描述
    #[serde(default)]
    pub folder_description: String,
}

/// 分享信息。
///
/// `get_share_info` 返回的 `info` 字段展开。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareInfo {
    /// 是否为新版域名
    #[serde(default)]
    pub is_newd: Option<String>,
    /// 文件/文件夹 ID
    #[serde(default)]
    pub f_id: Option<String>,
    /// 新版分享 URL
    #[serde(default)]
    pub new_url: Option<String>,
    /// 提取码
    #[serde(default)]
    pub pwd: Option<String>,
}

/// `heri://` 分享码负载（兼容旧版本 `SharePayload`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharePayload {
    /// 文件名
    pub n: String,
    /// MD5
    pub m: String,
    /// 人类可读大小
    pub s: String,
    /// 分片数
    pub c: u32,
    /// 分享 URL
    pub l: String,
    /// 提取码
    pub p: String,
}
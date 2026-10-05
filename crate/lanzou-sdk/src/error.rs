//! 统一错误类型与 `Result` 别名。
//!
//! 蓝奏云 API 返回的失败信息可能来自网络层、HTTP 状态、JSON 业务字段 `zt != 1`、
//! 正则解析失败等多种原因，本模块将其统一为 [`LanzouError`]。

use std::fmt;

/// 蓝奏云 SDK 的统一错误类型。
#[derive(Debug)]
pub enum LanzouError {
    /// 底层 `reqwest` 错误
    Http(reqwest::Error),
    /// URL 解析失败
    Url(url::ParseError),
    /// 非 UTF-8 响应体
    Utf8(std::string::FromUtf8Error),
    /// JSON 反序列化失败
    Json(serde_json::Error),
    /// IO 错误
    Io(std::io::Error),
    /// 蓝奏云业务错误，附带服务器返回的中文/英文提示
    Api(String),
    /// 登录已失效
    NotLoggedIn,
    /// 资源未找到
    NotFound(String),
    /// 用户输入 / 参数错误
    Invalid(String),
    /// 任意其他错误
    Other(String),
}

impl fmt::Display for LanzouError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LanzouError::Http(e) => write!(f, "网络错误: {e}"),
            LanzouError::Url(e) => write!(f, "URL 解析失败: {e}"),
            LanzouError::Utf8(e) => write!(f, "响应非 UTF-8: {e}"),
            LanzouError::Json(e) => write!(f, "JSON 解析失败: {e}"),
            LanzouError::Io(e) => write!(f, "IO 错误: {e}"),
            LanzouError::Api(msg) => write!(f, "蓝奏云业务错误: {msg}"),
            LanzouError::NotLoggedIn => write!(f, "蓝奏云未登录或会话已失效"),
            LanzouError::NotFound(what) => write!(f, "未找到资源: {what}"),
            LanzouError::Invalid(msg) => write!(f, "参数无效: {msg}"),
            LanzouError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for LanzouError {}

impl From<reqwest::Error> for LanzouError {
    fn from(e: reqwest::Error) -> Self {
        LanzouError::Http(e)
    }
}

impl From<url::ParseError> for LanzouError {
    fn from(e: url::ParseError) -> Self {
        LanzouError::Url(e)
    }
}

impl From<serde_json::Error> for LanzouError {
    fn from(e: serde_json::Error) -> Self {
        LanzouError::Json(e)
    }
}

impl From<std::io::Error> for LanzouError {
    fn from(e: std::io::Error) -> Self {
        LanzouError::Io(e)
    }
}

impl From<std::string::FromUtf8Error> for LanzouError {
    fn from(e: std::string::FromUtf8Error) -> Self {
        LanzouError::Utf8(e)
    }
}

/// 蓝奏云 SDK 的 `Result` 别名。
pub type Result<T> = std::result::Result<T, LanzouError>;
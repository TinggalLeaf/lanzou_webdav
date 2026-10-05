//! 会话持久化与账号管理。
//!
//! 蓝奏云登录后返回的两个 Cookie（`ylogin` + `phpdisk_info`）是后续所有
//! 已登录请求的凭证，本模块提供：
//!
//! - [`Session`] 结构体：JSON 序列化友好
//! - [`save_session`] / [`load_session`]：把会话写入 / 读出本地文件
//! - 登录 / 注册流程（见 [`crate::client::LanzouCloud`]）

use std::fs;
use std::path::Path;

use crate::error::{LanzouError, Result};
use crate::model::Session;

/// 把已登录会话保存到磁盘（JSON 格式）。
///
/// 适合直接绑定到 Docker volume，例如：
/// ```text
/// /data/account.json
/// ```
pub fn save_session(path: impl AsRef<Path>, session: &Session) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(LanzouError::Io)?;
    }
    let json = serde_json::to_string_pretty(session).map_err(LanzouError::Json)?;
    fs::write(path, json).map_err(LanzouError::Io)?;
    Ok(())
}

/// 从磁盘读取已登录会话。
///
/// 文件不存在或解析失败时返回 [`LanzouError::NotFound`] / [`LanzouError::Json`]。
pub fn load_session(path: impl AsRef<Path>) -> Result<Session> {
    let path = path.as_ref();
    if !path.exists() {
        return Err(LanzouError::NotFound(format!("{}", path.display())));
    }
    let data = fs::read_to_string(path).map_err(LanzouError::Io)?;
    let session: Session = serde_json::from_str(&data).map_err(LanzouError::Json)?;
    Ok(session)
}

/// 删除已保存的会话。
///
/// 该函数只在文件存在时尝试删除，不存在视为成功。
pub fn clear_session(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    if path.exists() {
        fs::remove_file(path).map_err(LanzouError::Io)?;
    }
    Ok(())
}

// 重新导出以保持原有导入路径兼容
pub use crate::model::AccountCredential;

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;

    #[test]
    fn session_round_trip() {
        let path = temp_dir().join("lanzou_test_account.json");
        let s = Session {
            ylogin: "abc".into(),
            phpdisk_info: "def".into(),
            account: "13800000000".into(),
        };
        save_session(&path, &s).unwrap();
        let loaded = load_session(&path).unwrap();
        assert_eq!(loaded.ylogin, "abc");
        assert_eq!(loaded.account, "13800000000");
        clear_session(&path).unwrap();
    }

    #[test]
    fn load_missing_returns_not_found() {
        let path = temp_dir().join("definitely_not_existing_file_xyz.json");
        let err = load_session(&path).unwrap_err();
        assert!(matches!(err, LanzouError::NotFound(_)));
    }
}
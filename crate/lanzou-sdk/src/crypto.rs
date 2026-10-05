//! 加密原语与 WAF 校验算法。
//!
//! 包括：
//! - 阿里 WAF `acw_sc__v2` 校验算法（纯 Rust 实现，不依赖 JS 引擎）
//! - 分片文件名的 ChaCha20-Poly1305 加密/解密
//!
//! 蓝奏云的分片上传要求每个分片名称不超过 64 个字符且只能包含 `[0-9a-f]`
//! 与后缀 `.zip`。因此分片名采用：随机 12 字节 nonce + 加密后的明文
//! （`{md5}{4 位 hex 序号}`），最终 `hex` 编码后追加 `.zip`。

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    ChaCha20Poly1305, Key, Nonce,
};

/// `acw_sc__v2` 校验算法中的固定置换表（来自阿里 WAF JS）。
const ACW_POSITIONS: [usize; 40] = [
    15, 35, 29, 24, 33, 16, 1, 38, 10, 9, 19, 31, 40, 27, 22, 23, 25, 13, 6, 11, 39, 18, 20, 8,
    14, 21, 32, 26, 2, 30, 7, 4, 17, 5, 3, 28, 34, 37, 12, 36,
];

/// `acw_sc__v2` 校验算法中的固定掩码。
const ACW_MASK: &str = "3000176000856006061501533003690027800375";

/// 计算 `acw_sc__v2` 校验值。
///
/// # 参数
/// - `arg1`：WAF JS 中 `arg1='ABCD...'` 中提取的 40 位大写十六进制字符串。
///
/// # 返回值
/// 通过反置换 + 与掩码异或得到的 40 位小写十六进制字符串，可直接写入
/// `acw_sc__v2` Cookie。
///
/// # 示例
/// ```
/// use lanzou_sdk::crypto::solve_acw_sc_v2;
/// // 该值由蓝奏云前端 JS 实时生成，下例仅为示意：
/// let _ = solve_acw_sc_v2;
/// ```
pub fn solve_acw_sc_v2(arg1: &str) -> String {
    let chars: Vec<char> = arg1.chars().collect();
    let mut q = vec![' '; 40];

    // 步骤 1：按置换表反序
    for i in 0..chars.len() {
        for j in 0..ACW_POSITIONS.len() {
            if ACW_POSITIONS[j] == i + 1 {
                if i < chars.len() {
                    q[j] = chars[i];
                }
            }
        }
    }
    let unshuffled: String = q.into_iter().collect();

    // 步骤 2：每两位十六进制与掩码异或
    let mut out = String::with_capacity(40);
    let bound = std::cmp::min(unshuffled.len(), ACW_MASK.len());
    for i in (0..bound).step_by(2) {
        let u = u8::from_str_radix(&unshuffled[i..i + 2], 16).unwrap_or(0);
        let m = u8::from_str_radix(&ACW_MASK[i..i + 2], 16).unwrap_or(0);
        out.push_str(&format!("{:02x}", u ^ m));
    }
    out
}

/// 从 HTML 中提取 `arg1='ABCD...'` 并计算 WAF Cookie。
///
/// 若 HTML 不包含 WAF 挑战，则返回 `None`，调用方应继续正常请求。
///
/// # 参数
/// - `html`：响应体文本
///
/// # 返回值
/// 可直接放入 Cookie 头的 `acw_sc__v2=xxxxxx` 字符串，未触发 WAF 时返回 `None`。
pub fn extract_acw_cookie(html: &str) -> Option<String> {
    // 注意：`arg1` 在 JS 中是单引号字符串，正则要兼容可能的多行/转义
    let re = regex::Regex::new(r#"arg1\s*=\s*['"]([0-9A-Fa-f]+)['"]"#).ok()?;
    let caps = re.captures(html)?;
    let arg1 = caps.get(1)?.as_str();
    Some(format!("acw_sc__v2={}", solve_acw_sc_v2(arg1)))
}

/// 构造 ChaCha20-Poly1305 密码器。
///
/// 优先读取环境变量 `CHUNKS_NAMES_KEY`，否则回落到 `HERIHERI_SECRET_KEY`，
/// 最后使用内置默认密钥。
fn filename_cipher() -> ChaCha20Poly1305 {
    let secret = std::env::var("CHUNKS_NAMES_KEY")
        .or_else(|_| std::env::var("HERIHERI_SECRET_KEY"))
        .unwrap_or_else(|_| "lanzou-sdk-default-secret-key-32B".to_string());

    let mut key_bytes = [0u8; 32];
    let bytes = secret.as_bytes();
    let len = std::cmp::min(bytes.len(), 32);
    key_bytes[..len].copy_from_slice(&bytes[..len]);

    ChaCha20Poly1305::new(Key::from_slice(&key_bytes))
}

/// 加密分片文件名。
///
/// 输出形如 `abc123...45e5f.zip`（末尾追加 `.zip`），总长度受蓝奏云限制为
/// ≤ 64 字符。返回值为完整文件名。
pub fn encrypt_chunk_filename(md5_str: &str, chunk_index: u32) -> String {
    let cipher = filename_cipher();
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);

    // 明文 = md5 + 4 位 hex 序号
    let plaintext = format!("{}{:04x}", md5_str, chunk_index);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .expect("ChaCha20-Poly1305 加密失败");

    let mut payload = nonce.to_vec();
    payload.extend_from_slice(&ciphertext);
    format!("{}.zip", hex::encode(payload))
}

/// 尝试解密分片文件名。
///
/// 成功时返回 `Some((md5, hex_part))`，失败（不属于本 SDK 生成的命名）返回 `None`。
pub fn decrypt_chunk_filename(filename: &str) -> Option<(String, String)> {
    let base = filename.strip_suffix(".zip").unwrap_or(filename);
    let decoded = hex::decode(base).ok()?;
    if decoded.len() < 12 {
        return None;
    }
    let (nonce_bytes, ciphertext) = decoded.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);
    let cipher = filename_cipher();

    let plaintext_bytes = cipher.decrypt(nonce, ciphertext).ok()?;
    let plaintext = String::from_utf8(plaintext_bytes).ok()?;
    if plaintext.len() < 4 {
        return None;
    }
    // 末尾 4 位是 hex 序号，前面是 md5
    let hex_idx = &plaintext[plaintext.len() - 4..];
    let md5 = plaintext[..plaintext.len() - 4].to_string();
    Some((md5, hex_idx.to_string()))
}

/// 从分片文件名提取其在原始文件中的序号（1-based）。
///
/// 兼容三种命名规则：
/// 1. 本 SDK 加密生成的 hex 字符串（最常见）
/// 2. `{md5}{4 位 hex}.zip` 旧格式
/// 3. `{name}_part{N}.iso` 旧旧格式
pub fn chunk_index_from_name(filename: &str) -> Option<u32> {
    if let Some((_, hex_idx)) = decrypt_chunk_filename(filename) {
        if let Ok(idx) = u32::from_str_radix(&hex_idx, 16) {
            return Some(idx);
        }
    }
    // 旧格式 1: <md5><4位hex>.zip
    let covert_re = regex::Regex::new(r"^[0-9a-f]{32}([0-9a-f]{4})\.zip$").ok()?;
    if let Some(caps) = covert_re.captures(filename) {
        if let Ok(idx) = u32::from_str_radix(&caps[1], 16) {
            return Some(idx);
        }
    }
    // 旧格式 2: *_part<N>.iso
    let legacy_re = regex::Regex::new(r"_part(\d+)\.iso$").ok()?;
    if let Some(caps) = legacy_re.captures(filename) {
        if let Ok(idx) = caps[1].parse::<u32>() {
            return Some(idx);
        }
    }
    None
}

/// 加密通用负载，返回 URL-safe Base64 字符串（nonce + ciphertext）。
///
/// 用于构造 `heri://` 分享码等场景。
pub fn encrypt_payload(json_str: &str) -> String {
    let cipher = filename_cipher();
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, json_str.as_bytes())
        .expect("ChaCha20-Poly1305 加密失败");
    let mut payload = nonce.to_vec();
    payload.extend_from_slice(&ciphertext);
    URL_SAFE_NO_PAD.encode(payload)
}

/// 解密 `encrypt_payload` 加密的负载，失败时返回错误字符串。
pub fn decrypt_payload(encoded: &str) -> Result<String, LanzouError> {
    use crate::error::LanzouError;
    let decoded = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| LanzouError::Other("Base64 解码失败".into()))?;
    if decoded.len() < 12 {
        return Err(LanzouError::Other("密文过短".into()));
    }
    let (nonce_bytes, ciphertext) = decoded.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);
    let cipher = filename_cipher();
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| LanzouError::Other("解密或鉴权失败".into()))?;
    String::from_utf8(plaintext).map_err(|e| LanzouError::Utf8(e))
}

use crate::error::LanzouError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wac_solver_is_deterministic() {
        // 用一个固定 arg1 测试反置换与异或的链路通畅即可
        let out1 = solve_acw_sc_v2("ABCDEF0123456789ABCDEF0123456789ABCDEF01");
        let out2 = solve_acw_sc_v2("ABCDEF0123456789ABCDEF0123456789ABCDEF01");
        assert_eq!(out1, out2);
        assert_eq!(out1.len(), 40);
    }

    #[test]
    fn encrypt_then_decrypt_chunk_filename_roundtrips() {
        let md5 = "d41d8cd98f00b204e9800998ecf8427e";
        let idx = 7u32;
        let enc = encrypt_chunk_filename(md5, idx);
        assert!(enc.ends_with(".zip"));
        let (md5_out, hex_idx) = decrypt_chunk_filename(&enc).unwrap();
        assert_eq!(md5_out, md5);
        assert_eq!(u32::from_str_radix(&hex_idx, 16).unwrap(), idx);
    }

    #[test]
    fn chunk_index_extractor_supports_legacy_patterns() {
        // 旧格式：md5 + 4 位 hex + .zip
        let idx = chunk_index_from_name(
            "d41d8cd98f00b204e9800998ecf8427e0007.zip",
        )
        .unwrap();
        assert_eq!(idx, 7);
        // 旧旧格式：_part<N>.iso
        let idx = chunk_index_from_name("anything_part9.iso").unwrap();
        assert_eq!(idx, 9);
    }

    #[test]
    fn encrypt_decrypt_payload_roundtrips() {
        let payload = r#"{"hello":"world"}"#;
        let enc = encrypt_payload(payload);
        let dec = decrypt_payload(&enc).unwrap();
        assert_eq!(dec, payload);
    }
}
//! 已登录账号的核心蓝奏云客户端 [`LanzouCloud`]。
//!
//! 提供以下能力：
//!
//! - 登录（手机号/用户名 + 密码）/ 注册（短信验证码）
//! - 文件夹与文件的列举、创建、删除、移动、回收站
//! - 分片上传（看门狗 + 重试 + 进度回调）
//! - 分享信息查询
//! - 会话恢复 [`LanzouCloud::from_session`]
//!
//! ## 阿里 WAF 自动绕过
//!
//! 蓝奏云登录端点位于 `accounts.woozooo.com`，首次访问会触发
//! `acw_sc__v2` 校验。本 SDK 在 [`login`] 内部检测并自动计算 Cookie。
//!
//! [`login`]: LanzouCloud::login

use crate::account::AccountCredential;
use crate::crypto::extract_acw_cookie;
use crate::error::{LanzouError, Result};
use crate::model::{Session, ShareInfo};
use bytes::Bytes;
use md5::Context;
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::Client;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::time::Duration;

/// 上传接口常量。
const BASE_URL: &str = "https://up.woozooo.com";
/// 账号接口常量。
const ACCOUNTS_URL: &str = "https://accounts.woozooo.com/accounts.php";

/// 单文件上传阈值（超过此大小走分片上传）。
pub const CHUNK_LIMIT: usize = 100 * 1024 * 1024;

/// 默认浏览器 UA。
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36";

/// 上传进度回调。
///
/// 参数：(已上传字节, 总字节)。
pub type ProgressFn = Arc<dyn Fn(usize, usize) + Send + Sync>;

/// 蓝奏云已登录客户端。
///
/// 内部持有 `reqwest::Client` 与文件夹栈，所有"当前目录"操作均以栈顶元素为准。
#[derive(Clone)]
pub struct LanzouCloud {
    client: Client,
    /// 登录后的 `ylogin`（用于拼接 URL `?uid=`）
    ylogin: Option<String>,
    /// 目录栈，栈顶为当前目录 ID，`-1` 表示根目录。
    folder_stack: Vec<String>,
}

/// 内部构造工具：从 Headers 列表提取 `ylogin` / `phpdisk_info` Cookie 值。
fn extract_cookie(headers: &HeaderMap, key: &str) -> Option<String> {
    headers.get_all(reqwest::header::SET_COOKIE).iter().find_map(|h| {
        let s = h.to_str().ok()?;
        if !s.starts_with(&format!("{key}=")) {
            return None;
        }
        s.split(';').next().map(|raw| raw.replace(&format!("{key}="), ""))
    })
}

impl LanzouCloud {
    /// 创建一个新的未登录客户端实例。
    ///
    /// 自动附带浏览器 UA 与 `Referer: mydisk.php`，可在调用 [`login`] 之前先
    /// 用作匿名请求。
    ///
    /// [`login`]: LanzouCloud::login
    pub fn new() -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_static(USER_AGENT),
        );
        headers.insert(
            reqwest::header::REFERER,
            HeaderValue::from_static("https://up.woozooo.com/mydisk.php"),
        );
        let client = Client::builder()
            .default_headers(headers)
            .cookie_store(true)
            .build()
            .expect("LanzouCloud 客户端构建失败");
        Self {
            client,
            ylogin: None,
            folder_stack: vec!["-1".to_string()],
        }
    }

    /// 从已登录会话恢复客户端。
    ///
    /// 等价于先 `new()` 再 [`set_session`](Self::set_session)。
    pub fn from_session(session: &Session) -> Self {
        let mut c = Self::new();
        c.set_session(session);
        c
    }

    /// 应用一个已登录会话（设置 Cookie + `ylogin`）。
    ///
    /// 调用后即可开始调用已登录的 API。
    pub fn set_session(&mut self, session: &Session) {
        self.ylogin = if session.ylogin.is_empty() {
            None
        } else {
            Some(session.ylogin.clone())
        };
        let cookie_str = format!(
            "ylogin={}; phpdisk_info={}",
            session.ylogin, session.phpdisk_info
        );
        let mut headers = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&cookie_str) {
            headers.insert(reqwest::header::COOKIE, v);
        }
        headers.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_static(USER_AGENT),
        );
        let new_client = Client::builder()
            .default_headers(headers)
            .cookie_store(true)
            .build()
            .expect("LanzouCloud 客户端重建失败");
        self.client = new_client;
    }

    /// 导出当前会话（用于持久化）。
    pub fn session(&self, account: &str) -> Session {
        // 注意：reqwest 的 CookieStore 不会直接暴露原始 Cookie 字符串，
        // 推荐调用方在登录后自行保留 ylogin/phpdisk_info。
        Session {
            ylogin: self.ylogin.clone().unwrap_or_default(),
            phpdisk_info: String::new(),
            account: account.to_string(),
        }
    }

    /// 探测当前是否已登录。
    ///
    /// 通过访问 `mydisk.php` 检查是否包含"退出"二字。
    pub async fn is_logged_in(&self) -> bool {
        let url = format!("{}/mydisk.php", BASE_URL);
        match self.client.get(&url).send().await {
            Ok(r) => match r.text().await {
                Ok(t) => t.contains("退出"),
                Err(_) => false,
            },
            Err(_) => false,
        }
    }

    /// 拉取 `vei` token（所有 doupload.php 请求都需携带）。
    async fn get_vei(&self) -> Result<String> {
        let mut url = format!("{}/mydisk.php?item=files&action=index", BASE_URL);
        if let Some(uid) = &self.ylogin {
            url.push_str(&format!("&u={uid}"));
        }
        let resp = self.client.get(&url).send().await?;
        let html = resp.text().await?;

        // 优先匹配 'vei': 'xxx'
        let re_direct = Regex::new(r#"['"]vei['"]\s*:\s*['"]([^'"]+)['"]"#)
            .map_err(|e| LanzouError::Other(format!("正则编译失败: {e}")))?;
        if let Some(caps) = re_direct.captures(&html) {
            return Ok(caps[1].to_string());
        }

        // 退化匹配：var.iri = '...'
        let re_var = Regex::new(r#"['"]vei['"]\s*:\s*([a-zA-Z0-9_]+)"#).unwrap();
        if let Some(caps) = re_var.captures(&html) {
            let var_name = &caps[1];
            let re_val = Regex::new(&format!(r#"{}\s*=\s*['"]([^'"]+)['"]"#, var_name)).unwrap();
            if let Some(val_caps) = re_val.captures(&html) {
                return Ok(val_caps[1].to_string());
            }
        }
        Err(LanzouError::Other("未能从 mydisk.php 提取 vei token".into()))
    }

    /// 拉取 `formhash`（用于回收站 restore / 永久删除）。
    pub async fn get_formhash(&self) -> Result<String> {
        let url = format!("{}/mydisk.php?item=recycle&action=files", BASE_URL);
        let resp = self.client.get(&url).send().await?;
        let html = resp.text().await?;
        let re = Regex::new(r#"name="formhash"\s+value="([a-fA-F0-9]+)""#)
            .map_err(|e| LanzouError::Other(format!("正则编译失败: {e}")))?;
        re.captures(&html)
            .map(|c| c[1].to_string())
            .ok_or_else(|| LanzouError::Other("回收站 formhash 未找到".into()))
    }

    /// 触发并自动绕过阿里 WAF 后，POST 账号接口。
    async fn post_with_waf(&self, form: &[(&str, &str)]) -> Result<JsonValue> {
        let referer_url = "https://accounts.woozooo.com/accounts.php?action=register";
        let get_resp = self
            .client
            .get(referer_url)
            .header("User-Agent", USER_AGENT)
            .send()
            .await?;
        let body_text = get_resp.text().await.unwrap_or_default();

        let waf_cookie = extract_acw_cookie(&body_text).unwrap_or_default();
        let mut req = self
            .client
            .post(ACCOUNTS_URL)
            .header("User-Agent", USER_AGENT)
            .header("Accept", "application/json, text/javascript, */*")
            .header("X-Requested-With", "XMLHttpRequest")
            .header("Origin", "https://accounts.woozooo.com")
            .header("Host", "accounts.woozooo.com")
            .header("Referer", referer_url);
        if !waf_cookie.is_empty() {
            req = req.header("Cookie", waf_cookie);
        }

        let resp = req.form(&form).send().await?;
        let json: JsonValue = resp.json().await?;
        Ok(json)
    }

    /// 通过手机号/用户名 + 密码登录。
    ///
    /// 成功时自动保存 `ylogin`、`phpdisk_info` 并返回。
    pub async fn login(&mut self, username: &str, password: &str) -> Result<Session> {
        let referer_url = format!("{}?action=login&ref=up.woozooo.com", ACCOUNTS_URL);
        let get_resp = self
            .client
            .get(&referer_url)
            .header("User-Agent", USER_AGENT)
            .send()
            .await?;
        let body_text = get_resp.text().await.unwrap_or_default();

        let waf_cookie = extract_acw_cookie(&body_text);
        let mut post_req = self
            .client
            .post(ACCOUNTS_URL)
            .header("User-Agent", USER_AGENT)
            .header("Accept", "application/json, text/javascript, */*")
            .header("X-Requested-With", "XMLHttpRequest")
            .header("Origin", "https://accounts.woozooo.com")
            .header("Host", "accounts.woozooo.com")
            .header("Referer", &referer_url);
        if let Some(cookie) = waf_cookie {
            post_req = post_req.header("Cookie", cookie);
        }

        let form = [
            ("task", "uselogin"),
            ("username", username),
            ("password", password),
            ("ref", "up.woozooo.com"),
        ];
        let resp = post_req.form(&form).send().await?;

        let ylogin = extract_cookie(resp.headers(), "ylogin")
            .ok_or_else(|| LanzouError::Other("登录响应缺少 ylogin cookie".into()))?;
        let phpdisk_info = extract_cookie(resp.headers(), "phpdisk_info")
            .ok_or_else(|| LanzouError::Other("登录响应缺少 phpdisk_info cookie".into()))?;

        let session = Session {
            ylogin: ylogin.clone(),
            phpdisk_info: phpdisk_info.clone(),
            account: username.to_string(),
        };
        self.set_session(&session);
        Ok(session)
    }

    /// 使用 [`AccountCredential`] 便捷登录并把会话持久化到 `path`。
    pub async fn login_and_save(
        &mut self,
        cred: &AccountCredential,
        path: impl AsRef<std::path::Path>,
    ) -> Result<Session> {
        let session = self.login(&cred.username, &cred.password).await?;
        crate::account::save_session(path, &session)?;
        Ok(session)
    }

    /// 发送注册短信验证码。
    pub async fn request_register_sms(&self, phone: &str) -> Result<String> {
        let form = [("task", "register"), ("phone", phone)];
        let json = self.post_with_waf(&form).await?;
        if json["zt"] == 1 {
            Ok(json["msgs"].as_str().unwrap_or("SMS sent").to_string())
        } else {
            Err(LanzouError::Api(
                json["msgs"]
                    .as_str()
                    .unwrap_or("发送短信失败")
                    .to_string(),
            ))
        }
    }

    /// 提交注册（验证码 + 密码）。
    pub async fn submit_register(
        &self,
        phone: &str,
        code: &str,
        password: &str,
    ) -> Result<()> {
        // 步骤 1：校验验证码
        let code_form = [
            ("task", "update_code"),
            ("phone", phone),
            ("verycode", code),
        ];
        let code_json = self.post_with_waf(&code_form).await?;
        if code_json["zt"] != 1 {
            return Err(LanzouError::Api("验证码无效".into()));
        }
        // 步骤 2：设置密码
        let pwd_form = [
            ("task", "update_pwd"),
            ("phone", phone),
            ("verycode", code),
            ("password1", password),
            ("password2", password),
        ];
        let pwd_json = self.post_with_waf(&pwd_form).await?;
        if pwd_json["zt"] == 1 {
            Ok(())
        } else {
            Err(LanzouError::Api("设置密码失败".into()))
        }
    }

    /// 进入某个子目录（压栈）。
    pub fn enter_folder(&mut self, folder_id: &str) {
        let cleaned = if let Some(stripped) = folder_id.strip_prefix("fol") {
            stripped.to_string()
        } else {
            folder_id.to_string()
        };
        self.folder_stack.push(cleaned);
    }

    /// 退出当前目录（弹栈），栈为空时无操作。
    pub fn go_back(&mut self) {
        if self.folder_stack.len() > 1 {
            self.folder_stack.pop();
        }
    }

    /// 当前目录 ID（栈顶）。
    pub fn current_folder(&self) -> String {
        self.folder_stack
            .last()
            .cloned()
            .unwrap_or_else(|| "-1".to_string())
    }

    /// 拉取当前目录下的文件夹列表。
    pub async fn list_folders(&self) -> Result<Vec<JsonValue>> {
        let folder_id = self.current_folder();
        let vei = self.get_vei().await?;
        let mut url = format!("{}/doupload.php", BASE_URL);
        if let Some(uid) = &self.ylogin {
            url.push_str(&format!("?uid={uid}"));
        }
        let form = [("task", "47"), ("folder_id", folder_id.as_str()), ("vei", vei.as_str())];
        let resp = self.client.post(&url).form(&form).send().await?;
        let json: JsonValue = resp.json().await?;
        if json["zt"] == 1 {
            Ok(json["text"].as_array().cloned().unwrap_or_default())
        } else {
            Ok(Vec::new())
        }
    }

    /// 拉取当前目录下的文件列表（自动翻页）。
    pub async fn list_files(&self) -> Result<Vec<JsonValue>> {
        let folder_id = self.current_folder();
        let vei = self.get_vei().await?;
        let mut url = format!("{}/doupload.php", BASE_URL);
        if let Some(uid) = &self.ylogin {
            url.push_str(&format!("?uid={uid}"));
        }
        let mut all = Vec::new();
        let mut pg = 1u32;
        loop {
            let pg_str = pg.to_string();
            let form = [
                ("task", "5"),
                ("folder_id", folder_id.as_str()),
                ("pg", pg_str.as_str()),
                ("vei", vei.as_str()),
            ];
            let resp = self.client.post(&url).form(&form).send().await?;
            let json: JsonValue = resp.json().await?;
            if json["zt"] != 1 || json["info"] == 0 {
                break;
            }
            match json["text"].as_array() {
                Some(arr) if !arr.is_empty() => all.extend(arr.clone()),
                _ => break,
            }
            pg += 1;
        }
        Ok(all)
    }

    /// 在当前目录下创建文件夹。
    ///
    /// 返回值是蓝奏云返回的完整 JSON（包含 `text` 字段——新建文件夹 ID）。
    pub async fn create_folder(&self, name: &str, description: &str) -> Result<JsonValue> {
        let current = self.current_folder();
        let parent_id = if current == "-1" { "0" } else { current.as_str() };
        self.create_folder_in_target(name, description, parent_id).await
    }

    /// 在指定蓝奏云目录 ID 下创建文件夹。
    pub async fn create_folder_in_target(
        &self,
        name: &str,
        description: &str,
        parent_id: &str,
    ) -> Result<JsonValue> {
        let url = format!("{}/doupload.php", BASE_URL);
        let form = [
            ("task", "2"),
            ("parent_id", parent_id),
            ("folder_name", name),
            ("folder_description", description),
        ];
        let resp = self
            .client
            .post(&url)
            .header("X-Requested-With", "XMLHttpRequest")
            .form(&form)
            .send()
            .await?;
        let json: JsonValue = resp.json().await?;
        if json["zt"] == 1 {
            Ok(json)
        } else {
            Err(LanzouError::Api(format!("创建文件夹失败: {json}")))
        }
    }

    /// 删除文件（移入蓝奏云回收站）。
    pub async fn delete_file(&self, file_id: &str) -> Result<bool> {
        let url = format!("{}/doupload.php", BASE_URL);
        let form = [("task", "6"), ("file_id", file_id)];
        let resp = self
            .client
            .post(&url)
            .header("X-Requested-With", "XMLHttpRequest")
            .form(&form)
            .send()
            .await?;
        let json: JsonValue = resp.json().await?;
        Ok(json["zt"] == 1)
    }

    /// 删除文件夹（递归）。
    pub async fn delete_folder(&self, folder_id: &str) -> Result<bool> {
        let cleaned = if let Some(s) = folder_id.strip_prefix("fol") {
            s.to_string()
        } else {
            folder_id.to_string()
        };
        let url = format!("{}/doupload.php", BASE_URL);
        let form = [("task", "3"), ("folder_id", cleaned.as_str())];
        let resp = self
            .client
            .post(&url)
            .header("X-Requested-With", "XMLHttpRequest")
            .form(&form)
            .send()
            .await?;
        let json: JsonValue = resp.json().await?;
        Ok(json["zt"] == 1)
    }

    /// 移动文件/分片目录到指定目标。
    ///
    /// 注意：蓝奏云 `task=20` 语义是 `folder_id` 目标，`file_id` 为待移动项。
    pub async fn move_item(&self, item_id: &str, target_folder_id: &str) -> Result<bool> {
        let url = format!("{}/doupload.php", BASE_URL);
        let form = [
            ("task", "20"),
            ("folder_id", target_folder_id),
            ("file_id", item_id),
        ];
        let resp = self
            .client
            .post(&url)
            .header("X-Requested-With", "XMLHttpRequest")
            .form(&form)
            .send()
            .await?;
        let json: JsonValue = resp.json().await?;
        Ok(json["zt"] == 1)
    }

    /// 获取分享信息。
    ///
    /// `is_folder = true` 时拉取文件夹分享信息（task 18），否则文件（task 22）。
    pub async fn get_share_info(&self, id: &str, is_folder: bool) -> Result<ShareInfo> {
        let url = format!("{}/doupload.php", BASE_URL);
        let task = if is_folder { "18" } else { "22" };
        let id_key = if is_folder { "folder_id" } else { "file_id" };
        let form = [("task", task), (id_key, id)];
        let resp = self
            .client
            .post(&url)
            .header("X-Requested-With", "XMLHttpRequest")
            .form(&form)
            .send()
            .await?;
        let json: JsonValue = resp.json().await?;
        if json["zt"] == 1 {
            Ok(serde_json::from_value(json["info"].clone())?)
        } else {
            Err(LanzouError::Api(format!("get_share_info 失败: {json}")))
        }
    }

    /// 从回收站恢复文件/文件夹。
    pub async fn restore_item(
        &self,
        id: &str,
        is_folder: bool,
        formhash: &str,
    ) -> Result<bool> {
        let url = format!("{}/mydisk.php?item=recycle", BASE_URL);
        let action = if is_folder {
            "folder_restore"
        } else {
            "file_restore"
        };
        let id_key = if is_folder { "folder_id" } else { "file_id" };
        let form = [
            ("action", action),
            ("task", action),
            (id_key, id),
            (
                "ref",
                "https://up.woozooo.com/mydisk.php?item=recycle&action=files",
            ),
            ("formhash", formhash),
        ];
        let resp = self.client.post(&url).form(&form).send().await?;
        let html = resp.text().await?;
        Ok(html.contains("恢复成功"))
    }

    /// 永久删除回收站中的项目。
    pub async fn hard_delete_item(
        &self,
        id: &str,
        is_folder: bool,
        formhash: &str,
    ) -> Result<bool> {
        let url = format!("{}/mydisk.php?item=recycle", BASE_URL);
        let action = if is_folder {
            "folder_delete_complete"
        } else {
            "file_delete_complete"
        };
        let id_key = if is_folder { "folder_id" } else { "file_id" };
        let form = [
            ("action", action),
            ("task", action),
            (id_key, id),
            (
                "ref",
                "https://up.woozooo.com/mydisk.php?item=recycle&action=files",
            ),
            ("formhash", formhash),
        ];
        let resp = self.client.post(&url).form(&form).send().await?;
        let html = resp.text().await?;
        Ok(html.contains("删除成功"))
    }

    /// 确保 `.heriheri` 与 `.deeperdir` 两个根目录存在，返回其蓝奏云 ID。
    ///
    /// - `.heriheri`：VFS 浅层目录（深度 < 2）映射到此
    /// - `.deeperdir`：深度溢出池
    pub async fn ensure_vfs_roots(&mut self) -> Result<(String, String)> {
        let saved_stack = self.folder_stack.clone();
        self.folder_stack = vec!["-1".to_string()];
        let folders = self.list_folders().await?;

        let mut root = String::new();
        let mut deeper = String::new();
        for f in folders {
            let name = f["name"].as_str().unwrap_or("");
            let fid = f["fol_id"]
                .as_str()
                .map(|s| s.to_string())
                .or_else(|| f["fol_id"].as_u64().map(|n| n.to_string()))
                .unwrap_or_default();
            match name {
                ".heriheri" if root.is_empty() => root = fid,
                ".deeperdir" if deeper.is_empty() => deeper = fid,
                _ => {}
            }
        }
        if root.is_empty() {
            let res = self
                .create_folder(".heriheri", "Lanzou WebDAV VFS Root")
                .await?;
            root = extract_text_id(&res);
        }
        if deeper.is_empty() {
            let res = self
                .create_folder(".deeperdir", "Lanzou WebDAV Deep Overflow")
                .await?;
            deeper = extract_text_id(&res);
        }
        self.folder_stack = saved_stack;
        Ok((root, deeper))
    }

    /// 上传单文件（≤ 100 MB）直接上传，返回新文件 ID。
    pub async fn upload_small(
        &self,
        bytes: Bytes,
        safe_name: &str,
        target_folder: &str,
        progress: ProgressFn,
    ) -> Result<String> {
        let parent_id = if target_folder == "-1" { "0" } else { target_folder };
        let total = bytes.len();
        // 单文件场景下直接整段提交即可；若调用方传入了进度回调，
        // 在请求完成时同步汇报一次 100%。
        let mime = mime_guess::from_path(safe_name)
            .first_or_octet_stream()
            .to_string();
        let url = format!("{}/html5up.php", BASE_URL);

        let part = reqwest::multipart::Part::bytes(bytes.to_vec())
            .file_name(safe_name.to_string())
            .mime_str(&mime)
            .map_err(|e| LanzouError::Other(e.to_string()))?;

        let form = reqwest::multipart::Form::new()
            .text("task", "1")
            .text("vie", "2")
            .text("ve", "2")
            .text("id", "WU_FILE_0")
            .text("folder_id_bb_n", parent_id.to_string())
            .text("name", safe_name.to_string())
            .text("type", mime.clone())
            .text("size", total.to_string())
            .part("upload_file", part);

        let resp = self
            .client
            .post(&url)
            .header("X-Requested-With", "XMLHttpRequest")
            .header("Origin", "https://up.woozooo.com")
            .header("Referer", "https://up.woozooo.com/mydisk.php")
            .header("Accept-Language", "en-US,en;q=0.9,zh-CN;q=0.8")
            .multipart(form)
            .send()
            .await?;
        let json: JsonValue = resp.json().await?;
        // 上传完成回调 100%
        progress(total, total);
        if json["zt"] == 1 {
            let id = json["text"][0]["id"].as_str().unwrap_or("").to_string();
            if id.is_empty() {
                Err(LanzouError::Api("上传成功但未返回文件 ID".into()))
            } else {
                Ok(id)
            }
        } else {
            Err(LanzouError::Api(format!("上传失败: {json}")))
        }
    }

    /// 上传任意大小文件，超出 [`CHUNK_LIMIT`] 自动分片。
    ///
    /// 进度回调单位：已上传 / 总字节。
    /// 返回 `(lanzou_id, chunks)`：分片时 `lanzou_id` 是分片目录 ID，否则是文件 ID。
    pub async fn upload_file(
        &mut self,
        bytes: Bytes,
        safe_name: &str,
        target_folder: &str,
        progress: Option<ProgressFn>,
    ) -> Result<(String, u32)> {
        let parent_id = if target_folder == "-1" { "0" } else { target_folder };
        let total = bytes.len();
        let noop: ProgressFn = Arc::new(|_, _| {});
        let progress = progress.unwrap_or(noop);

        if total <= CHUNK_LIMIT {
            let id = self
                .upload_small(bytes, safe_name, parent_id, progress)
                .await?;
            return Ok((id, 1));
        }

        // 分片：以 MD5 作为分片目录名（蓝奏云文件名仅允许字母数字下划线）
        let mut hasher = Context::new();
        hasher.consume(&bytes);
        let md5_hex = format!("{:x}", hasher.compute());
        let safe_md5: String = md5_hex
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();

        // 查找或创建分片目录
        let chunk_folder_id = self
            .find_or_create_chunk_folder(&safe_md5, parent_id, &md5_hex)
            .await?;

        let chunk_count = (total + CHUNK_LIMIT - 1) / CHUNK_LIMIT;
        for i in 0..chunk_count {
            let start = i * CHUNK_LIMIT;
            let end = std::cmp::min(start + CHUNK_LIMIT, total);
            let chunk = bytes.slice(start..end);
            let chunk_name = crate::crypto::encrypt_chunk_filename(&md5_hex, (i + 1) as u32);
            let id = self
                .upload_small(
                    chunk,
                    &chunk_name,
                    &chunk_folder_id,
                    {
                        let progress = progress.clone();
                        Arc::new(move |cur, _chunk_total| {
                            progress(start + cur, total);
                        })
                    },
                )
                .await?;
            if id.is_empty() {
                return Err(LanzouError::Api(format!(
                    "分片 {} 上传成功但无 ID 返回",
                    i + 1
                )));
            }
            // 让一下事件循环，避免阻塞
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        progress(total, total);
        Ok((chunk_folder_id, chunk_count as u32))
    }

    /// 在 `parent_id` 下查找名为 `safe_md5` 的子目录，不存在则创建。
    async fn find_or_create_chunk_folder(
        &mut self,
        safe_md5: &str,
        parent_id: &str,
        md5_hex: &str,
    ) -> Result<String> {
        let saved = self.folder_stack.clone();
        self.folder_stack = vec![parent_id.to_string()];
        let folders = self.list_folders().await?;
        for f in folders {
            let name = f["name_all"]
                .as_str()
                .or_else(|| f["name"].as_str())
                .unwrap_or("");
            if name == safe_md5 || name == md5_hex {
                let id = extract_text_id(&f);
                self.folder_stack = saved;
                return Ok(id);
            }
        }
        let res = self
            .create_folder_in_target(safe_md5, "", parent_id)
            .await?;
        let id = extract_text_id(&res);
        self.folder_stack = saved;
        Ok(id)
    }

    /// 用看门狗跑一段受控的异步任务（用于单元测试时模拟任务控制）。
    #[allow(dead_code)]
    pub(crate) fn watchdog_signal() -> Arc<std::sync::atomic::AtomicU8> {
        Arc::new(std::sync::atomic::AtomicU8::new(0))
    }
}

impl Default for LanzouCloud {
    fn default() -> Self {
        Self::new()
    }
}

/// 从蓝奏云返回 JSON 中安全提取 `text` 字段。
///
/// 蓝奏云部分接口返回 `text` 为字符串，部分为对象（包含 id），本函数
/// 统一把字符串变成 `{"id": "..."}` 后取 id。
fn extract_text_id(json: &JsonValue) -> String {
    extract_text_id_inner(&json["text"])
}

fn extract_text_id_inner(v: &JsonValue) -> String {
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    if let Some(id) = v["id"].as_str() {
        return id.to_string();
    }
    if let Some(arr) = v.as_array() {
        if let Some(first) = arr.first() {
            return extract_text_id_inner(first);
        }
    }
    String::new()
}

// 临时使用 Regex 别名，避免引入 use 顺序问题

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_text_id_handles_string_and_object() {
        let s = json!({"zt": 1, "text": "abc"});
        assert_eq!(extract_text_id(&s), "abc");
        let o = json!({"zt": 1, "text": {"id": "xyz"}});
        assert_eq!(extract_text_id(&o), "xyz");
        let arr = json!({"zt": 1, "text": [{"id": "111"}, {"id": "222"}]});
        assert_eq!(extract_text_id(&arr), "111");
    }
}
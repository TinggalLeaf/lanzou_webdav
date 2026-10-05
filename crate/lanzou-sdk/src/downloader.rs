//! 公开分享链接的直链解析器（无需登录）。
//!
//! 蓝奏云在分享页会经过两层挑战：
//!
//! 1. **阿里 WAF (`acw_sc__v2`)**：首次访问分享页时弹出，本 SDK 用纯 Rust 实现
//!    见 [`crate::crypto::solve_acw_sc_v2`]。
//! 2. **下载页验证 (`/file/...` → `ajax.php`)**：需要先 GET `/file/xxx` 拿到
//!    `file` 与 `sign`，再 POST `ajax.php`，等待 2.1 秒后即可拿到直链。
//!
//! 该模块封装以上流程，对外只暴露 [`LanzouDownloader::get_lanzou_direct_link`] 与
//! [`LanzouDownloader::get_lanzou_folder_metadata`]。

use crate::crypto::solve_acw_sc_v2;
use crate::error::{LanzouError, Result};
use regex::Regex;
use reqwest::cookie::Jar;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Client, Url};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

/// 阿里 WAF 校验算法注入到 Cookie 中的延迟（毫秒），需与蓝奏云前端保持一致。
const FINAL_AJAX_DELAY_MS: u64 = 2100;

/// 默认 User-Agent，模拟 Windows Chrome。
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/148.0.0.0 Safari/537.36";

/// 公开分享链接解析器（自带 Cookie 容器）。
#[derive(Clone)]
pub struct LanzouDownloader {
    /// 复用同一 Cookie 容器以跨请求保持 `acw_sc__v2` 等状态。
    client: Client,
    /// Cookie 容器，与 `client` 共享。
    #[allow(dead_code)]
    jar: Arc<Jar>,
}

impl Default for LanzouDownloader {
    fn default() -> Self {
        Self::new()
    }
}

impl LanzouDownloader {
    /// 构造一个下载器实例。
    ///
    /// 内部已配置好 Chrome User-Agent 与 Cookie 持久化。
    pub fn new() -> Self {
        let jar = Arc::new(Jar::default());
        let mut headers = HeaderMap::new();
        headers.insert(
            "User-Agent",
            HeaderValue::from_static(USER_AGENT),
        );
        headers.insert(
            "Accept-Language",
            HeaderValue::from_static("en-US,en;q=0.9,zh-CN;q=0.8,zh;q=0.7"),
        );
        let client = Client::builder()
            .cookie_provider(Arc::clone(&jar))
            .default_headers(headers)
            .build()
            .expect("LanzouDownloader 客户端构建失败");
        Self { client, jar }
    }

    /// 解决蓝奏云分享页的 WAF 挑战（如果存在）。
    ///
    /// 返回经过 WAF 解锁后的 HTML 文本。
    async fn solve_waf(&self, html: &str, url: &str) -> Result<String> {
        // 阿里 WAF 的特征 JS：var arg1='XXXXXXXX...'
        let re = Regex::new(r#"var\s+arg1\s*=\s*['"]([0-9A-Fa-f]+)['"]"#)
            .map_err(|e| LanzouError::Other(format!("正则编译失败: {e}")))?;
        if let Some(caps) = re.captures(html) {
            let arg1 = caps.get(1).unwrap().as_str();
            let cookie_value = solve_acw_sc_v2(arg1);
            let parsed = Url::parse(url)
                .map_err(|e| LanzouError::Other(format!("URL 解析失败: {e}")))?;
            let domain = parsed.domain().unwrap_or("");

            // 直接放入 Cookie 容器，下一次 GET 即可带上
            self.jar.add_cookie_str(
                &format!("acw_sc__v2={}; Domain={}; Path=/", cookie_value, domain),
                &parsed,
            );

            // 重新 GET 一次获取解锁后的页面
            let resp = self.client.get(url).send().await?;
            let html2 = resp.text().await?;
            return Ok(html2);
        }
        Ok(html.to_string())
    }

    /// 解析文件分享页的类型 A（带密码 + isngis）。
    async fn parse_type_a(
        &self,
        html: &str,
        share_url: &str,
        password: Option<&str>,
    ) -> Result<(String, String)> {
        // 先去除 /* ... */ 注释以避免误匹配
        let re_comments = Regex::new(r"(?s)/\*.*?\*/").unwrap();
        let clean = re_comments.replace_all(html, "");

        let file_id_re =
            Regex::new(r#"url\s*:\s*['"]/ajaxfile\.php\?file=(\d+)['"]"#).unwrap();
        let isngis_re = Regex::new(r"var\s+isngis\s*=\s*'([^']*)'").unwrap();

        let file_id = file_id_re
            .captures(&clean)
            .ok_or_else(|| LanzouError::NotFound("文件分享页：file_id".into()))?[1]
            .to_string();
        // 取最后一个非空 isngis（最稳妥）
        let sign = isngis_re
            .captures_iter(&clean)
            .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
            .filter(|s| !s.is_empty())
            .last()
            .ok_or_else(|| LanzouError::NotFound("文件分享页：isngis 签名".into()))?;

        let ajax_url = Url::parse(share_url)?
            .join(&format!("/ajaxfile.php?file={}", file_id))?
            .to_string();

        let mut form: HashMap<&str, String> = HashMap::new();
        form.insert("action", "downprocess".into());
        form.insert("sign", sign);
        form.insert("kd", "1".into());
        form.insert("p", password.unwrap_or("").to_string());

        let resp = self
            .client
            .post(&ajax_url)
            .headers(ajax_headers(share_url))
            .form(&form)
            .send()
            .await?;
        let result: Value = resp.json().await?;

        if result.get("zt").and_then(|z| z.as_i64()) != Some(1) {
            return Err(LanzouError::Api(format!("第一步失败: {result}")));
        }
        let dom = result
            .get("dom")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        let url_path = result
            .get("url")
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_string();
        Ok((dom, url_path))
    }

    /// 解析文件分享页的类型 B（无密码 + wp_sign）。
    async fn parse_type_b(
        &self,
        html: &str,
        share_url: &str,
        _password: Option<&str>,
    ) -> Result<(String, String)> {
        let fn_re = Regex::new(r#"src=["'](/fn\?[^"']+)"#).unwrap();
        let fn_match = fn_re
            .captures(html)
            .ok_or_else(|| LanzouError::NotFound("Type B：/fn? URL".into()))?[1]
            .to_string();
        let fn_url = Url::parse(share_url)?
            .join(&fn_match)?
            .to_string();

        let mut headers = HeaderMap::new();
        headers.insert("Referer", HeaderValue::from_str(share_url).unwrap());
        let resp = self
            .client
            .get(&fn_url)
            .headers(headers)
            .send()
            .await?;
        let html_fn = resp.text().await?;

        let ajaxdata_re = Regex::new(r"var\s+ajaxdata\s*=\s*'([^']+)'").unwrap();
        let wp_sign_re = Regex::new(r"var\s+wp_sign\s*=\s*'([^']+)'").unwrap();
        let file_re = Regex::new(r#"url\s*:\s*['"]/ajaxfile\.php\?file=(\d+)['"]"#).unwrap();

        let ajaxdata = ajaxdata_re
            .captures(&html_fn)
            .ok_or_else(|| LanzouError::NotFound("Type B：ajaxdata".into()))?[1]
            .to_string();
        let wp_sign = wp_sign_re
            .captures(&html_fn)
            .ok_or_else(|| LanzouError::NotFound("Type B：wp_sign".into()))?[1]
            .to_string();
        let file_id = file_re
            .captures(&html_fn)
            .ok_or_else(|| LanzouError::NotFound("Type B：file_id".into()))?[1]
            .to_string();

        let killdns = Regex::new(r"(var\s+killdns|killdns\s*=)").unwrap();
        let kd = if killdns.is_match(&html_fn) { "1" } else { "0" };

        let ajax_url = Url::parse(share_url)?
            .join(&format!("/ajaxfile.php?file={}", file_id))?
            .to_string();

        let mut form: HashMap<&str, String> = HashMap::new();
        form.insert("action", "downprocess".into());
        form.insert("websignkey", ajaxdata.clone());
        form.insert("signs", ajaxdata);
        form.insert("sign", wp_sign);
        form.insert("websign", "2".into());
        form.insert("kd", kd.into());
        form.insert("ves", "1".into());

        let resp = self
            .client
            .post(&ajax_url)
            .headers(ajax_headers(&fn_url))
            .form(&form)
            .send()
            .await?;
        let result: Value = resp.json().await?;

        if result.get("zt").and_then(|z| z.as_i64()) != Some(1) {
            return Err(LanzouError::Api(format!("Type B 第一步失败: {result}")));
        }
        let dom = result
            .get("dom")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        let url_path = result
            .get("url")
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_string();
        Ok((dom, url_path))
    }

    /// 第一步：把分享链接解析为 `(dom, url_path)` 二元组。
    async fn resolve_file_page(
        &self,
        share_url: &str,
        password: Option<&str>,
    ) -> Result<(String, String)> {
        let resp = self.client.get(share_url).send().await?;
        let html_initial = resp.text().await?;
        let html = self.solve_waf(&html_initial, share_url).await?;

        // 优先 Type A：含密码 + isngis 签名
        if html.contains("/ajaxfile.php?file=")
            && html.replace(' ', "").contains("action':'downprocess'")
            && html.contains("var isngis")
        {
            return self.parse_type_a(&html, share_url, password).await;
        }
        // Type B：无密码，wp_sign
        if html.contains("/fn?") && !html.contains("wp_sign") {
            return self.parse_type_b(&html, share_url, password).await;
        }
        Err(LanzouError::Invalid(format!(
            "无法识别的蓝奏云分享页类型: {share_url}"
        )))
    }

    /// 第二步：拿到 `dom` 与 `url_path` 后，获取真实 CDN 直链。
    async fn get_direct_link_from_dom_url(
        &self,
        dom: &str,
        url_path: &str,
        referer: &str,
    ) -> Result<String> {
        let download_page_url = format!("{}/file/{}", dom.trim_end_matches('/'), url_path);
        let parsed = Url::parse(&download_page_url)
            .map_err(|e| LanzouError::Other(format!("URL 解析失败: {e}")))?;
        let domain = parsed.domain().unwrap_or("");

        self.jar.add_cookie_str(
            &format!("down_ip=1; Domain={}; Path=/", domain),
            &parsed,
        );

        let mut headers = HeaderMap::new();
        headers.insert("Referer", HeaderValue::from_str(referer).unwrap());
        let resp = self
            .client
            .get(&download_page_url)
            .headers(headers)
            .send()
            .await?;
        let html2 = resp.text().await?;

        // 去除注释
        let re_comments = Regex::new(r"(?s)/\*.*?\*/").unwrap();
        let mut clean = re_comments.replace_all(&html2, "").to_string();
        let re_line = Regex::new(r"//.*?\n").unwrap();
        clean = re_line.replace_all(&clean, "\n").to_string();

        let re_second = Regex::new(
            r"'file'\s*:\s*'([^']+)'\s*,\s*'el'\s*:\s*[a-zA-Z0-9_]+\s*,\s*'sign'\s*:\s*'([^']+)'",
        )
        .unwrap();

        let mut file_val = String::new();
        let mut sign2 = String::new();
        for caps in re_second.captures_iter(&clean) {
            file_val = caps[1].to_string();
            sign2 = caps[2].to_string();
        }
        if file_val.is_empty() || sign2.is_empty() {
            return Err(LanzouError::NotFound(
                "下载页：file / sign 校验参数".into(),
            ));
        }

        let final_ajax = parsed.join("ajax.php").map_err(|e| {
            LanzouError::Other(format!("URL 拼接失败: {e}"))
        })?;

        let mut form: HashMap<&str, String> = HashMap::new();
        form.insert("file", file_val);
        form.insert("el", "2".into());
        form.insert("sign", sign2);

        sleep(Duration::from_millis(FINAL_AJAX_DELAY_MS)).await;

        let resp = self
            .client
            .post(final_ajax.as_str())
            .headers(ajax_headers(&download_page_url))
            .form(&form)
            .send()
            .await?;
        let final_result: Value = resp.json().await?;
        if final_result.get("zt").and_then(|z| z.as_i64()) != Some(1) {
            return Err(LanzouError::Api(format!(
                "最终验证失败: {final_result}"
            )));
        }
        final_result
            .get("url")
            .and_then(|u| u.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| LanzouError::NotFound("最终直链 url 字段".into()))
    }

    /// 一站式接口：分享链接 → CDN 直链。
    pub async fn get_lanzou_direct_link(
        &self,
        share_url: &str,
        password: Option<&str>,
    ) -> Result<String> {
        let (dom, url_path) = self.resolve_file_page(share_url, password).await?;
        self.get_direct_link_from_dom_url(&dom, &url_path, share_url)
            .await
    }

    /// 拉取分享文件夹下的全部文件元数据（含分片文件夹场景）。
    ///
    /// 返回 `Vec<Value>`，每个元素至少包含 `id`、`name_all`、`size`、`time` 字段。
    pub async fn get_lanzou_folder_metadata(
        &self,
        folder_url: &str,
        password: Option<&str>,
    ) -> Result<Vec<Value>> {
        let resp = self.client.get(folder_url).send().await?;
        let html_initial = resp.text().await?;
        let html = self.solve_waf(&html_initial, folder_url).await?;

        let fid_re = Regex::new(r"'fid'\s*:\s*(\d+)").unwrap();
        let uid_re = Regex::new(r"'uid'\s*:\s*'(\d+)'").unwrap();
        let puid_re = Regex::new(r"'puid'\s*:\s*'([^']+)'").unwrap();

        let fid = fid_re
            .captures(&html)
            .ok_or_else(|| LanzouError::NotFound("fid".into()))?[1]
            .to_string();
        let uid = uid_re
            .captures(&html)
            .ok_or_else(|| LanzouError::NotFound("uid".into()))?[1]
            .to_string();
        let puid = puid_re
            .captures(&html)
            .ok_or_else(|| LanzouError::NotFound("puid".into()))?[1]
            .to_string();

        // 解析 file() 函数体，找出 t/k 变量名
        let script_block =
            Regex::new(r"(?s)function\s+file\s*\(\s*\)\s*\{(.*?)function\s+more\s*\(\s*\)")
                .unwrap();
        let script = script_block
            .captures(&html)
            .ok_or_else(|| LanzouError::NotFound("file() 函数块".into()))?[1]
            .to_string();

        let t_var_re = Regex::new(r"'t'\s*:\s*(\w+)\s*[,\}]").unwrap();
        let k_var_re = Regex::new(r"'k'\s*:\s*(\w+)\s*[,\}]").unwrap();
        let t_var = t_var_re
            .captures(&script)
            .ok_or_else(|| LanzouError::NotFound("t 变量名".into()))?[1]
            .to_string();
        let k_var = k_var_re
            .captures(&script)
            .ok_or_else(|| LanzouError::NotFound("k 变量名".into()))?[1]
            .to_string();

        let t_val_re = Regex::new(&format!(
            r"var\s+{}\s*=\s*'([^']+)'",
            regex::escape(&t_var)
        ))
        .unwrap();
        let k_val_re = Regex::new(&format!(
            r"var\s+{}\s*=\s*'([^']+)'",
            regex::escape(&k_var)
        ))
        .unwrap();
        let t = t_val_re
            .captures(&html)
            .ok_or_else(|| LanzouError::NotFound("t 值".into()))?[1]
            .to_string();
        let k = k_val_re
            .captures(&html)
            .ok_or_else(|| LanzouError::NotFound("k 值".into()))?[1]
            .to_string();

        let parsed = Url::parse(folder_url)?;
        let mut all_files = Vec::new();
        let mut pg = 1;
        loop {
            let mut form: HashMap<&str, String> = HashMap::new();
            form.insert("lx", "2".into());
            form.insert("fid", fid.clone());
            form.insert("uid", uid.clone());
            form.insert("puid", puid.clone());
            form.insert("pg", pg.to_string());
            form.insert("rep", "0".into());
            form.insert("t", t.clone());
            form.insert("k", k.clone());
            form.insert("up", "1".into());
            form.insert("ls", "1".into());
            form.insert("pwd", password.unwrap_or("").to_string());

            let url = parsed
                .join(&format!("/filemoreajax.php?file={}", fid))?
                .to_string();

            let resp = self
                .client
                .post(&url)
                .headers(ajax_headers(folder_url))
                .form(&form)
                .send()
                .await?;
            let data: Value = resp.json().await?;
            if data.get("zt").and_then(|z| z.as_i64()) != Some(1) {
                break;
            }
            if let Some(files) = data.get("text").and_then(|t| t.as_array()) {
                all_files.extend(files.clone());
                if files.len() < 50 {
                    break;
                }
            } else {
                break;
            }
            sleep(Duration::from_millis(FINAL_AJAX_DELAY_MS)).await;
            pg += 1;
        }
        Ok(all_files)
    }

    /// 拉取分享文件夹下所有文件的直链列表（高并发解析）。
    ///
    /// 仅在需要"文件夹批量下载"等场景使用，普通文件下载请用
    /// [`get_lanzou_direct_link`](Self::get_lanzou_direct_link)。
    #[allow(dead_code)]
    pub async fn get_lanzou_folder_links(
        &self,
        folder_url: &str,
        password: Option<&str>,
        concurrency: usize,
    ) -> Result<Vec<Value>> {
        use futures::stream::{self, StreamExt};
        let all_files = self.get_lanzou_folder_metadata(folder_url, password).await?;
        let parsed = Url::parse(folder_url)?;
        let base = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));
        let dl = self.clone();

        let mut stream = stream::iter(all_files)
            .map(|file_info| {
                let base = base.clone();
                let dl = dl.clone();
                async move {
                    let id = file_info
                        .get("id")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = file_info
                        .get("name_all")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let size = file_info
                        .get("size")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    let time = file_info
                        .get("time")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    let file_share_url = format!("{}/{}", base, id);
                    match dl.get_lanzou_direct_link(&file_share_url, None).await {
                        Ok(direct) => json!({
                            "name": name,
                            "size": size,
                            "time": time,
                            "direct_url": direct,
                        }),
                        Err(e) => json!({
                            "name": name,
                            "size": size,
                            "time": time,
                            "direct_url": Value::Null,
                            "error": e.to_string(),
                        }),
                    }
                }
            })
            .buffer_unordered(concurrency.max(1));

        let mut results = Vec::new();
        while let Some(r) = stream.next().await {
            results.push(r);
        }
        Ok(results)
    }
}

/// 构造蓝奏云 AJAX 接口所需的浏览器头。
fn ajax_headers(referer: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    let parsed = Url::parse(referer).ok();
    let origin = parsed
        .as_ref()
        .map(|u| format!("{}://{}", u.scheme(), u.host_str().unwrap_or("")))
        .unwrap_or_default();
    h.insert("Accept", HeaderValue::from_static("application/json, text/javascript, */*"));
    h.insert(
        "Accept-Language",
        HeaderValue::from_static("en-US,en;q=0.9,zh-CN;q=0.8,zh;q=0.7"),
    );
    h.insert("Cache-Control", HeaderValue::from_static("no-cache"));
    h.insert(
        "Content-Type",
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    if !origin.is_empty() {
        h.insert("Origin", HeaderValue::from_str(&origin).unwrap_or(HeaderValue::from_static("")));
    }
    h.insert("Pragma", HeaderValue::from_static("no-cache"));
    if let Ok(v) = HeaderValue::from_str(referer) {
        h.insert("Referer", v);
    }
    h.insert("Sec-Fetch-Dest", HeaderValue::from_static("empty"));
    h.insert("Sec-Fetch-Mode", HeaderValue::from_static("cors"));
    h.insert("Sec-Fetch-Site", HeaderValue::from_static("same-origin"));
    h.insert("User-Agent", HeaderValue::from_static(USER_AGENT));
    h.insert(
        "X-Requested-With",
        HeaderValue::from_static("XMLHttpRequest"),
    );
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_include_referer_and_origin() {
        let h = ajax_headers("https://www.lanzoux.com/abcde");
        assert_eq!(
            h.get("Referer").unwrap().to_str().unwrap(),
            "https://www.lanzoux.com/abcde"
        );
        assert_eq!(
            h.get("Origin").unwrap().to_str().unwrap(),
            "https://www.lanzoux.com"
        );
    }
}
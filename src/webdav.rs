//! WebDAV 协议实现。
//!
//! ## 支持的方法
//!
//! - `OPTIONS`：返回服务器支持的 DAV 能力
//! - `PROPFIND`：列出目录或查询文件属性（类 Unix `ls -la`）
//! - `GET` / `HEAD`：流式下载（蓝奏云分享直链 → 字节流代理）
//! - `MKCOL`：创建目录
//! - `PUT`：上传文件（自动 MD5 → 蓝奏云秒传复用 / 分片上传）
//! - `DELETE`：把节点移入回收站（不真正删除）
//!
//! ## 路径 → VFS 节点
//!
//! `/dav/<URL 编码的路径>` 通过 `pid_stack + 名称匹配` 解析到 [`VfsNode`]。
//! 不存在的路径返回 404；同名冲突会自动加 `(1) (2)` 后缀（与 HeriHeri 行为一致）。

use axum::{
    body::Body,
    extract::State as AxumState,
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use bytes::Bytes;
use futures_util::StreamExt;
use lanzou_sdk::{
    model::{NodeType, ShareInfo},
    VfsNode, VfsTree,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, error, info};

use crate::state::AppState;

/// 直链缓存：VFS 节点 ID → (直链 URL, 失效时间)
#[derive(Clone)]
struct CachedLink {
    url: String,
    expires_at: Instant,
}

#[derive(Default)]
pub struct DirectLinkCache {
    inner: Mutex<HashMap<u64, CachedLink>>,
}

impl DirectLinkCache {
    pub fn new() -> Self {
        Self::default()
    }
}

/// 构造 axum 路由。
pub fn build_router(state: Arc<AppState>, cache: Arc<DirectLinkCache>) -> Router {
    Router::new()
        .route("/dav", axum::routing::any(handle_root_or_dav))
        .route("/dav/", axum::routing::any(handle_root_or_dav))
        .route("/dav/*path", axum::routing::any(handle_dav))
        .route("/health", get(health))
        .with_state((state, cache))
}

/// 启动 axum 服务并阻塞直到监听结束。
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: Arc<AppState>,
) -> anyhow::Result<()> {
    let cache = Arc::new(DirectLinkCache::new());
    let app = build_router(state, cache);
    info!("📡 axum 开始接受 HTTP 请求");
    axum::serve(listener, app).await?;
    Ok(())
}

/// `/health` 健康检查端点。
async fn health() -> &'static str {
    "OK"
}

/// 根路径：要求 Basic Auth，并把根路径视为 DAV 入口。
async fn handle_root_or_dav(
    AxumState((state, cache)): AxumState<(Arc<AppState>, Arc<DirectLinkCache>)>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    if !check_basic_auth(&headers, &state) {
        return unauthorized_response();
    }
    handle_dav_inner(state, cache, method, "/dav/", (headers, Body::empty())).await
}

/// 主入口：根据 HTTP 方法分发到具体处理器。
async fn handle_dav(
    AxumState((state, cache)): AxumState<(Arc<AppState>, Arc<DirectLinkCache>)>,
    method: Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    // Basic Auth 校验
    if !check_basic_auth(&headers, &state) {
        return unauthorized_response();
    }
    let p = uri.path();
    handle_dav_inner(state, cache, method, p, headers_with_body(headers, body)).await
}

/// 把 GET/HEAD 和其他方法统一封装成一个 Body 参数。
fn headers_with_body(headers: HeaderMap, body: Body) -> (HeaderMap, Body) {
    (headers, body)
}

/// 主入口：根据 HTTP 方法分发到具体处理器。
async fn handle_dav_inner(
    state: Arc<AppState>,
    cache: Arc<DirectLinkCache>,
    method: Method,
    path: &str,
    headers_or: (HeaderMap, Body),
) -> Response {
    let (headers, body) = headers_or;
    let p = path.strip_prefix("/dav").unwrap_or("");
    let p = p.strip_prefix('/').unwrap_or(p);
    let decoded_path = decode_url(p);

    debug!("[WEBDAV] {} {}", method, decoded_path);

    match method.as_str() {
        "OPTIONS" => handle_options(),
        "PROPFIND" => handle_propfind(&state, &decoded_path, &headers).await,
        "GET" | "HEAD" => {
            handle_get(state.clone(), cache.clone(), &decoded_path, &headers, method == Method::HEAD).await
        }
        "PUT" => handle_put(&state, &decoded_path, headers, body).await,
        "MKCOL" => handle_mkcol(&state, &decoded_path).await,
        "DELETE" => handle_delete(&state, &decoded_path).await,
        _ => (StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed").into_response(),
    }
}

/// 校验 HTTP Basic Auth 头部。
fn check_basic_auth(headers: &HeaderMap, state: &AppState) -> bool {
    let expected = format!(
        "Basic {}",
        BASE64.encode(format!("{}:{}", state.webdav_user, state.webdav_pass))
    );
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s == expected)
        .unwrap_or(false)
}

fn unauthorized_response() -> Response {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(
            "WWW-Authenticate",
            HeaderValue::from_static(r#"Basic realm="Lanzou WebDAV""#),
        )
        .body(Body::empty())
        .unwrap()
}

fn handle_options() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("Allow", "OPTIONS, GET, HEAD, PROPFIND, PUT, MKCOL, DELETE")
        .header("DAV", "1, 2")
        .header("MS-Author-Via", "DAV")
        .body(Body::empty())
        .unwrap()
}

/// URL 百分号解码。
fn decode_url(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars.next().unwrap_or('0');
            let h2 = chars.next().unwrap_or('0');
            if let Ok(b) = u8::from_str_radix(&format!("{h1}{h2}"), 16) {
                bytes.push(b);
            }
        } else if c == '+' {
            bytes.push(b' ');
        } else {
            let mut buf = [0; 4];
            for &b in c.encode_utf8(&mut buf).as_bytes() {
                bytes.push(b);
            }
        }
    }
    String::from_utf8(bytes).unwrap_or_else(|_| s.to_string())
}

/// URL 编码单段。
fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn quick_xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// 把路径按 `/` 切分，每段都解码。
fn split_path(p: &str) -> Vec<String> {
    p.split('/')
        .filter(|s| !s.is_empty())
        .map(decode_url)
        .collect()
}

/// 把人类可读的大小（如 "1.2 MB"）解析为字节。
fn parse_size(s: &str) -> u64 {
    let s = s.to_uppercase().replace(' ', "");
    if s.is_empty() || s == "-" {
        return 0;
    }
    let mut num = String::new();
    let mut unit = "";
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
        } else {
            unit = &s[num.len()..];
            break;
        }
    }
    let val = num.parse::<f64>().unwrap_or(0.0);
    let mul = match unit {
        "K" | "KB" => 1024.0,
        "M" | "MB" => 1024.0 * 1024.0,
        "G" | "GB" => 1024.0 * 1024.0 * 1024.0,
        "T" | "TB" => 1024.0_f64.powi(4),
        _ => 1.0,
    };
    (val * mul) as u64
}

/// 根据文件名猜 MIME。
fn content_type_for_name(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mpeg" | "mpg" => "video/mpeg",
        "ts" | "m2ts" => "video/mp2t",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" => "audio/ogg",
        "opus" => "audio/opus",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "txt" | "log" | "md" | "json" => "text/plain; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// 把路径解析到 `(父节点 PID, 末段文件名, 末段节点 ID)`。
///
/// 保留以备未来扩展；当前版本改用 [`resolve_node_id`]。
#[allow(dead_code)]
async fn resolve_path(
    state: &AppState,
    path: &[String],
) -> Option<(u64, String, Option<u64>)> {
    if path.is_empty() {
        return Some((0, String::new(), Some(0)));
    }
    let mut pid: u64 = 0;
    for (i, seg) in path.iter().enumerate() {
        let last = i + 1 == path.len();
        let vfs = state.vfs.lock().await;
        let tree = vfs.as_ref()?;
        let children = tree.list_dir(pid);
        // 自动重名
        let mut seen: HashMap<String, u32> = HashMap::new();
        let mut found: Option<(u64, NodeType, String)> = None;
        for child in children {
            let mut display = child.name.clone();
            let count = seen.entry(display.clone()).or_insert(0);
            if *count > 0 {
                if let Some(idx) = display.rfind('.') {
                    let (n, e) = display.split_at(idx);
                    display = format!("{n} ({count}){e}");
                } else {
                    display = format!("{display} ({count})");
                }
            }
            *count += 1;
            if display == *seg {
                found = Some((child.id, child.node_type, child.name));
                break;
            }
        }
        match found {
            Some((id, ntype, _)) => {
                if last {
                    return Some((pid, seg.clone(), Some(id)));
                }
                if ntype != NodeType::D {
                    return None;
                }
                pid = id;
            }
            None => return None,
        }
    }
    Some((pid, String::new(), None))
}

/// PROPFIND 主体实现。
async fn handle_propfind(state: &AppState, path: &str, headers: &HeaderMap) -> Response {
    let parts = split_path(path);
    let depth = headers
        .get("Depth")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("1");

    let vfs = state.vfs.lock().await;
    if vfs.is_none() {
        return (StatusCode::SERVICE_UNAVAILABLE, "VFS 未初始化").into_response();
    }
    drop(vfs);

    // 当前节点
    let (current_id, _is_dir) = match resolve_node_id(state, &parts).await {
        Some((id, dir)) => (id, dir),
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    let vfs = state.vfs.lock().await;
    let tree = match vfs.as_ref() {
        Some(t) => t,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "VFS 未初始化").into_response(),
    };

    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="utf-8" ?>
<D:multistatus xmlns:D="DAV:">"#,
    );

    if current_id == 0 {
        // 根目录自身
        append_prop_node(&mut xml, 0, "Root", &parts, true, 0, "2036-01-01T00:00:00Z");
        if depth == "1" {
            let children = tree.list_dir(0);
            for c in children {
                append_node(&mut xml, tree, c, &parts);
            }
        }
    } else if let Some(node) = tree.nodes.get(&current_id).cloned() {
        let display = display_name_for(tree, current_id);
        append_node(&mut xml, tree, node.clone(), &parts);
        let _ = display;
    } else {
        return StatusCode::NOT_FOUND.into_response();
    }

    xml.push_str("</D:multistatus>");
    Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from(xml))
        .unwrap()
}

fn append_node(xml: &mut String, tree: &VfsTree, node: VfsNode, parent_parts: &[String]) {
    // WebDAV 默认隐藏回收站项目，避免客户端看到"幽灵"条目
    if node.is_trashed || node.is_deleted {
        return;
    }
    let mut segs = parent_parts.to_vec();
    segs.push(node.name.clone());
    let size = parse_size(&node.size);
    append_prop_node(
        xml,
        node.id,
        &node.name,
        &segs,
        node.node_type == NodeType::D,
        size,
        &format_timestamp(node.time),
    );
    let _ = tree;
}

fn append_prop_node(
    xml: &mut String,
    id: u64,
    name: &str,
    segs: &[String],
    is_dir: bool,
    size: u64,
    lastmod: &str,
) {
    let href = format!(
        "/dav/{}",
        segs.iter()
            .map(|s| encode_segment(s))
            .collect::<Vec<_>>()
            .join("/")
    );
    let href = if is_dir && !href.ends_with('/') {
        format!("{href}/")
    } else {
        href
    };
    xml.push_str("<D:response>\n");
    xml.push_str(&format!("  <D:href>{}</D:href>\n", href));
    xml.push_str("  <D:propstat>\n");
    xml.push_str("    <D:prop>\n");
    xml.push_str(&format!(
        "      <D:displayname>{}</D:displayname>\n",
        quick_xml_escape(name)
    ));
    if is_dir {
        xml.push_str("      <D:resourcetype><D:collection/></D:resourcetype>\n");
    } else {
        xml.push_str("      <D:resourcetype/>\n");
        xml.push_str(&format!(
            "      <D:getcontentlength>{size}</D:getcontentlength>\n"
        ));
        xml.push_str(&format!(
            "      <D:getcontenttype>{}</D:getcontenttype>\n",
            content_type_for_name(name)
        ));
    }
    xml.push_str(&format!(
        "      <D:getlastmodified>{lastmod}</D:getlastmodified>\n"
    ));
    xml.push_str(&format!("      <D:creationdate>{lastmod}</D:creationdate>\n"));
    xml.push_str(&format!("      <D:id>{id}</D:id>\n"));
    xml.push_str("    </D:prop>\n");
    xml.push_str("    <D:status>HTTP/1.1 200 OK</D:status>\n");
    xml.push_str("  </D:propstat>\n");
    xml.push_str("</D:response>\n");
}

fn format_timestamp(ms: u64) -> String {
    let secs = ms / 1000;
    let Some(dt) = chrono::DateTime::from_timestamp(secs as i64, 0) else {
        return "Tue, 01 Jan 2036 00:00:00 GMT".into();
    };
    dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn display_name_for(_tree: &VfsTree, _id: u64) -> String {
    String::new()
}

/// 把 URL 路径解析到 VFS 节点 ID。
async fn resolve_node_id(state: &AppState, parts: &[String]) -> Option<(u64, bool)> {
    let vfs = state.vfs.lock().await;
    let tree = vfs.as_ref()?;
    let mut pid = 0u64;
    if parts.is_empty() {
        return Some((0, true));
    }
    for seg in parts {
        let children = tree.list_dir(pid);
        let mut seen: HashMap<String, u32> = HashMap::new();
        let mut found = None;
        for child in children {
            let mut display = child.name.clone();
            let count = seen.entry(display.clone()).or_insert(0);
            if *count > 0 {
                if let Some(idx) = display.rfind('.') {
                    let (n, e) = display.split_at(idx);
                    display = format!("{n} ({count}){e}");
                } else {
                    display = format!("{display} ({count})");
                }
            }
            *count += 1;
            if display == *seg {
                found = Some((child.id, child.node_type == NodeType::D));
                break;
            }
        }
        match found {
            Some((id, _is_dir)) => pid = id,
            None => return None,
        }
    }
    Some((pid, tree.nodes.get(&pid).map(|n| n.node_type == NodeType::D).unwrap_or(true)))
}

/// 流式 GET：通过蓝奏云分享直链下载，并按需转发 Range 请求。
async fn handle_get(
    state: Arc<AppState>,
    cache: Arc<DirectLinkCache>,
    path: &str,
    headers: &HeaderMap,
    head_only: bool,
) -> Response {
    let parts = split_path(path);
    let Some((vfs_id, is_dir)) = resolve_node_id(&state, &parts).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if is_dir {
        return (StatusCode::FORBIDDEN, "无法下载目录").into_response();
    }

    // 取出节点信息
    let (name, chunks, total_size, lanzou_id) = {
        let vfs = state.vfs.lock().await;
        let Some(tree) = vfs.as_ref() else {
            return (StatusCode::SERVICE_UNAVAILABLE, "VFS 未初始化").into_response();
        };
        let Some(node) = tree.nodes.get(&vfs_id).cloned() else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let chunks: u32 = node.chunks.parse().unwrap_or(1);
        (
            node.name.clone(),
            chunks,
            parse_size(&node.size),
            node.lanzou_id.clone(),
        )
    };

    // 取出分享信息
    let share_info = match fetch_share_info(&state, &lanzou_id).await {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("获取分享信息失败: {e}"),
            )
                .into_response();
        }
    };
    let (share_url, pwd) = match parse_share_url(&share_info) {
        Some(v) => v,
        None => return (StatusCode::BAD_GATEWAY, "分享 URL 为空").into_response(),
    };

    // 单文件 / 分片目录分支
    let mut all_chunks: Vec<(String, Option<String>)> = Vec::new();
    if chunks <= 1 {
        all_chunks.push((share_url, pwd));
    } else {
        match state
            .downloader
            .get_lanzou_folder_metadata(&share_url, pwd.as_deref())
            .await
        {
            Ok(_files) => {
                // 此分支占位：下方继续
            }
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    format!("获取分片元数据失败: {e}"),
                )
                    .into_response();
            }
        }
        // 真正获取分片列表
        match state
            .downloader
            .get_lanzou_folder_metadata(&share_url, pwd.as_deref())
            .await
        {
            Ok(files) => {
                let parsed = match url::Url::parse(&share_url) {
                    Ok(u) => u,
                    Err(e) => {
                        return (
                            StatusCode::BAD_GATEWAY,
                            format!("分享 URL 非法: {e}"),
                        )
                            .into_response();
                    }
                };
                let base = format!(
                    "{}://{}",
                    parsed.scheme(),
                    parsed.host_str().unwrap_or("")
                );
                for f in files {
                    let id = f.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    if id.is_empty() {
                        continue;
                    }
                    all_chunks.push((format!("{base}/{id}"), pwd.clone()));
                }
            }
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    format!("获取分片元数据失败: {e}"),
                )
                    .into_response();
            }
        }
    }

    if all_chunks.is_empty() {
        return (StatusCode::NO_CONTENT, "").into_response();
    }

    // 解析 Range
    let range_header = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let chunk_limit = lanzou_sdk::client::CHUNK_LIMIT as u64;
    let chunk_count = all_chunks.len() as u64;
    let (start, end) = match parse_range(range_header.as_deref(), total_size, chunk_count, chunk_limit) {
        Some(v) => v,
        None => return (StatusCode::RANGE_NOT_SATISFIABLE, "Range 越界").into_response(),
    };

    if head_only {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, (end - start + 1).to_string())
            .header(header::CONTENT_TYPE, content_type_for_name(&name))
            .body(Body::empty())
            .unwrap();
    }

    let state = state.clone();
    let cache = cache.clone();
    let body_stream = async_stream::stream! {
        let mut cursor = start;
        let chunk_limit = chunk_limit;
        while cursor <= end {
            let chunk_idx = (cursor / chunk_limit) as usize;
            let local_start = cursor % chunk_limit;
            let remaining_global = end - cursor + 1;
            let local_end = std::cmp::min(local_start + remaining_global - 1, chunk_limit - 1);
            if chunk_idx >= all_chunks.len() {
                break;
            }
            let (share_url_ref, pwd_ref) = &all_chunks[chunk_idx];
            // 取直链（带缓存）
            let direct_url = match resolve_cached_direct_link(
                &state,
                &cache,
                vfs_id,
                chunk_idx,
                share_url_ref,
                pwd_ref.as_deref(),
            ).await {
                Ok(u) => u,
                Err(e) => {
                    error!("HTTP 大宗：取直链失败 {e}");
                    break;
                }
            };
            // 拉取
            let client = reqwest::Client::builder()
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
                .build()
                .unwrap();
            let resp = match client
                .get(&direct_url)
                .header(header::RANGE.as_str(), format!("bytes={local_start}-{local_end}"))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    error!("上游下载失败: {e}");
                    break;
                }
            };
            let mut stream = resp.bytes_stream();
            while let Some(chunk_res) = stream.next().await {
                match chunk_res {
                    Ok(b) => {
                        cursor += b.len() as u64;
                        yield Ok::<Bytes, std::io::Error>(b);
                        if cursor > end {
                            break;
                        }
                    }
                    Err(e) => {
                        error!("上游读取错误: {e}");
                        break;
                    }
                }
            }
            if cursor > end {
                break;
            }
        }
    };

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, content_type_for_name(&name))
        .header(header::CONTENT_LENGTH, (end - start + 1).to_string());
    if range_header.is_some() {
        builder = builder
            .status(StatusCode::PARTIAL_CONTENT)
            .header(
                header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{total_size}"),
            );
    }
    builder
        .body(Body::from_stream(body_stream))
        .unwrap()
}

/// 解析 `bytes=start-end`，返回全局字节范围。
///
/// `chunk_count` 与 `chunk_size` 用于把"分片文件"的全局偏移换算回"实际字节"。
fn parse_range(
    header: Option<&str>,
    total_size: u64,
    chunk_count: u64,
    chunk_size: u64,
) -> Option<(u64, u64)> {
    let total_global = if chunk_count > 0 {
        chunk_count * chunk_size
    } else {
        total_size
    };
    let total = total_size.max(total_global);
    if total == 0 {
        return None;
    }
    match header {
        None => Some((0, total - 1)),
        Some(s) => {
            let s = s.strip_prefix("bytes=")?;
            let (start, end) = s.split_once('-')?;
            let (start, end) = if start.is_empty() {
                let suffix: u64 = end.parse().ok()?;
                if suffix == 0 || suffix > total {
                    return None;
                }
                (total - suffix, total - 1)
            } else {
                let st: u64 = start.parse().ok()?;
                let en: u64 = if end.is_empty() {
                    total - 1
                } else {
                    let parsed: u64 = end.parse().ok()?;
                    parsed.min(total - 1)
                };
                if st > en || st >= total {
                    return None;
                }
                (st, en)
            };
            Some((start, end))
        }
    }
}

/// 获取直链（带 4 分钟缓存）。
async fn resolve_cached_direct_link(
    state: &AppState,
    cache: &DirectLinkCache,
    vfs_id: u64,
    chunk_index: usize,
    share_url: &str,
    pwd: Option<&str>,
) -> anyhow::Result<String> {
    {
        let g = cache.inner.lock().await;
        if let Some(c) = g.get(&vfs_id) {
            if c.expires_at > Instant::now() && chunk_index == 0 {
                return Ok(c.url.clone());
            }
        }
    }
    let direct = state
        .downloader
        .get_lanzou_direct_link(share_url, pwd)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut g = cache.inner.lock().await;
    g.insert(
        vfs_id,
        CachedLink {
            url: direct.clone(),
            expires_at: Instant::now() + Duration::from_secs(240),
        },
    );
    let _ = chunk_index;
    Ok(direct)
}

/// 把蓝奏云 `ShareInfo` 提取成 (URL, pwd) 二元组。
fn parse_share_url(info: &ShareInfo) -> Option<(String, Option<String>)> {
    if let Some(u) = info.new_url.as_ref().filter(|s| !s.is_empty()) {
        return Some((u.clone(), info.pwd.clone()));
    }
    let dom = info.is_newd.as_deref().unwrap_or("");
    let fid = info.f_id.as_deref().unwrap_or("");
    if dom.is_empty() || fid.is_empty() {
        None
    } else {
        Some((format!("{dom}/{fid}"), info.pwd.clone()))
    }
}

async fn fetch_share_info(state: &AppState, lanzou_id: &str) -> anyhow::Result<ShareInfo> {
    let client = state.client.lock().await;
    // 仅文件直接查，文件夹/分片目录用 task 18
    let is_folder = lanzou_id.starts_with("alien://")
        || !lanzou_id.chars().all(|c| c.is_ascii_alphanumeric());
    let _ = is_folder;
    let info = match client.get_share_info(lanzou_id, true).await {
        Ok(i) => i,
        Err(_) => client
            .get_share_info(lanzou_id, false)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?,
    };
    Ok(info)
}

/// PUT 上传：自动 MD5 秒传 / 分片上传，并把节点加入 VFS。
async fn handle_put(state: &AppState, path: &str, _headers: HeaderMap, body: Body) -> Response {
    let parts = split_path(path);
    if parts.is_empty() {
        return (StatusCode::BAD_REQUEST, "PUT 必须指定文件名").into_response();
    }
    let parent_parts: Vec<String> = parts.iter().take(parts.len() - 1).cloned().collect();
    let file_name = parts.last().cloned().unwrap();

    let parent_id = match resolve_node_id(state, &parent_parts).await {
        Some((id, _)) => id,
        None => return (StatusCode::CONFLICT, "父目录不存在").into_response(),
    };

    // 读取完整 body（PUT 一般不会特别大，蓝奏云单文件上限 100MB）
    let bytes = match collect_body(body).await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("读取 body: {e}")).into_response(),
    };
    if bytes.is_empty() {
        return (StatusCode::NO_CONTENT, "空文件").into_response();
    }

    let progress: lanzou_sdk::uploader::ProgressCb = {
        let total = bytes.len();
        std::sync::Arc::new(move |loaded, _t| {
            debug!("上传进度：{loaded}/{total}");
        })
    };

    let mut client = state.client.lock().await;
    let parent_lanzou_id = match lanzou_id_for_vfs_node(state, parent_id).await {
        Some(id) => id,
        None => {
            return (StatusCode::CONFLICT, "父目录未关联蓝奏云 ID")
                .into_response()
        }
    };

    // 估算目标文件夹 ID：父目录 > 1 深度时使用 deeperdir
    let parent_depth = vfs_depth(state, parent_id).await;
    let target_folder = if parent_depth >= 2 {
        // 扁平化到 deeperdir
        match vfs_deeperdir_id(state).await {
            Some(id) => id,
            None => parent_lanzou_id,
        }
    } else {
        parent_lanzou_id
    };

    let mut uploader = lanzou_sdk::uploader::Uploader::new(&mut client);
    let result = match uploader
        .upload_bytes(&bytes, &file_name, &target_folder, Some(progress))
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("上传失败: {e}"),
            )
                .into_response();
        }
    };

    drop(uploader);
    drop(client);

    // 写 VFS
    let mut vfs = state.vfs.lock().await;
    if let Some(tree) = vfs.as_mut() {
        let chunks = result.chunks;
        tree.add_file(
            parent_id,
            &file_name,
            &result.lanzou_id,
            &bytes.len().to_string(),
            &result.md5,
            std::path::Path::new(&file_name)
                .extension()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
                .as_str(),
            chunks,
        );
        let _ = tree.save_local();
    }

    Response::builder()
        .status(StatusCode::CREATED)
        .header("Location", format!("/dav/{}", encode_segment(&file_name)))
        .body(Body::empty())
        .unwrap()
}

/// MKCOL：创建目录。
async fn handle_mkcol(state: &AppState, path: &str) -> Response {
    let parts = split_path(path);
    if parts.is_empty() {
        return (StatusCode::METHOD_NOT_ALLOWED, "不能在根目录上调用 MKCOL")
            .into_response();
    }
    let parent_parts: Vec<String> = parts.iter().take(parts.len() - 1).cloned().collect();
    let folder_name = parts.last().cloned().unwrap();
    let parent_id = match resolve_node_id(state, &parent_parts).await {
        Some((id, _)) => id,
        None => return (StatusCode::CONFLICT, "父目录不存在").into_response(),
    };
    let parent_depth = vfs_depth(state, parent_id).await;
    let target_folder = if parent_depth >= 2 {
        vfs_deeperdir_id(state).await.unwrap_or_default()
    } else {
        match lanzou_id_for_vfs_node(state, parent_id).await {
            Some(s) => s,
            None => return (StatusCode::CONFLICT, "父目录未关联蓝奏云 ID").into_response(),
        }
    };

    let client = state.client.lock().await;
    let res = match client
        .create_folder_in_target(&folder_name, "", &target_folder)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("创建目录失败: {e}"),
            )
                .into_response();
        }
    };
    drop(client);

    let new_id = if let Some(s) = res["text"].as_str() {
        s.to_string()
    } else if let Some(id) = res["text"]["id"].as_str() {
        id.to_string()
    } else {
        return (StatusCode::BAD_GATEWAY, "创建返回无 ID").into_response();
    };

    let mut vfs = state.vfs.lock().await;
    if let Some(tree) = vfs.as_mut() {
        tree.create_folder(parent_id, &folder_name, &new_id);
        let _ = tree.save_local();
    }
    Response::builder()
        .status(StatusCode::CREATED)
        .body(Body::empty())
        .unwrap()
}

/// DELETE：把节点移入 VFS 回收站（is_trashed=true）。
/// 蓝奏云物理文件不立即删除，等用户在回收站中"永久删除"时再清理。
async fn handle_delete(state: &AppState, path: &str) -> Response {
    let parts = split_path(path);
    let Some((id, _)) = resolve_node_id(state, &parts).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut vfs = state.vfs.lock().await;
    if let Some(tree) = vfs.as_mut() {
        tree.trash_node(id);
        let _ = tree.save_local();
    }
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap()
}

async fn collect_body(body: Body) -> anyhow::Result<Bytes> {
    let bytes = axum::body::to_bytes(body, 200 * 1024 * 1024)
        .await
        .map_err(|e| anyhow::anyhow!("读取 body 失败: {e}"))?;
    Ok(bytes)
}

async fn vfs_depth(state: &AppState, id: u64) -> usize {
    let vfs = state.vfs.lock().await;
    vfs.as_ref().map(|t| t.depth_of(id)).unwrap_or(0)
}

async fn vfs_deeperdir_id(state: &AppState) -> Option<String> {
    let vfs = state.vfs.lock().await;
    vfs.as_ref().and_then(|t| {
        if t.deeperdir_lanzou_id.is_empty() {
            None
        } else {
            Some(t.deeperdir_lanzou_id.clone())
        }
    })
}

async fn lanzou_id_for_vfs_node(state: &AppState, id: u64) -> Option<String> {
    if id == 0 {
        let vfs = state.vfs.lock().await;
        return vfs.as_ref().map(|t| t.root_lanzou_id.clone());
    }
    let vfs = state.vfs.lock().await;
    vfs.as_ref()
        .and_then(|t| t.nodes.get(&id).map(|n| n.lanzou_id.clone()))
}

/// 仅用于调试的占位函数，避免 unused 警告。
#[allow(dead_code)]
fn _check_method() -> Method {
    Method::GET
}
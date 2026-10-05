//! 蓝奏云之上的虚拟文件系统 (VFS)。
//!
//! 蓝奏云原生不暴露 `move` 接口，且不支持空文件夹，因此本 SDK 在上层
//! 构建了一个轻量 VFS（`VfsTree` + `VfsNode`），通过父子 ID (`pid`) 维护
//! 任意深度的目录结构，使用蓝奏云的两个根目录（`.heriheri` 与 `.deeperdir`）
//! 作为物理后备：
//!
//! - 浅层目录（深度 < 2）：物理映射到对应的蓝奏云子目录
//! - 深层目录：扁平化写入 `.deeperdir` 溢出池，仅靠 VFS 父子关系表达
//!
//! 该模型同时支持回收站 (`is_trashed`) 与墓碑 (`is_deleted`)，
//! 配合 [`VfsTree::merge_with`] 可以做简单的 CRDT 同步。

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{LanzouError, Result};
use crate::model::NodeType;

/// 单个虚拟文件/目录节点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VfsNode {
    /// 节点类型
    pub node_type: NodeType,
    /// 节点唯一 ID（VFS 内部编号，与蓝奏云无关）
    pub id: u64,
    /// 父节点 ID（0 表示根）
    pub pid: u64,
    /// 显示名称（明文）
    pub name: String,
    /// 对应蓝奏云的物理 ID（文件 ID 或文件夹 ID）
    /// 也可能是 `alien://<...>` 形式的分享符号链接
    pub lanzou_id: String,
    /// 最后修改时间（毫秒）
    pub time: u64,
    /// 人类可读大小（如 "1.2 MB"），目录为空字符串
    pub size: String,
    /// 文件 MD5（小写 32 位 hex），目录为空
    pub md5: String,
    /// 原始扩展名
    pub ext: String,
    /// 分片数（1 表示不分片，>1 表示分片目录）
    pub chunks: String,
    /// 是否在本地回收站
    #[serde(default)]
    pub is_trashed: bool,
    /// 是否已被墓碑化（不可恢复删除）
    #[serde(default)]
    pub is_deleted: bool,
}

/// 虚拟文件系统树，序列化为 TSV 文本持久化。
#[derive(Debug, Clone)]
pub struct VfsTree {
    /// 最后修改时间戳
    pub last_modified: u64,
    /// `.heriheri` 根目录的蓝奏云 ID
    pub root_lanzou_id: String,
    /// `.deeperdir` 溢出池的蓝奏云 ID
    pub deeperdir_lanzou_id: String,
    /// 所有节点（key = id）
    pub nodes: HashMap<u64, VfsNode>,
    /// 下一个可分配 ID
    pub next_id: u64,
    /// 本地持久化路径
    pub file_path: PathBuf,
}

impl VfsTree {
    /// 创建一个全新的空 VFS。
    ///
    /// # 参数
    /// - `root_lanzou_id`：`.heriheri` 根目录的蓝奏云 ID
    /// - `deeperdir_lanzou_id`：`.deeperdir` 溢出池的蓝奏云 ID
    /// - `file_path`：TSV 持久化文件路径
    pub fn new(root_lanzou_id: String, deeperdir_lanzou_id: String, file_path: PathBuf) -> Self {
        Self {
            last_modified: 0,
            root_lanzou_id,
            deeperdir_lanzou_id,
            nodes: HashMap::new(),
            next_id: 1,
            file_path,
        }
    }

    /// 将 VFS 状态持久化到本地文件。
    pub fn save_local(&self) -> Result<()> {
        let data = self.to_tsv();
        if let Some(parent) = self.file_path.parent() {
            fs::create_dir_all(parent).map_err(LanzouError::Io)?;
        }
        fs::write(&self.file_path, data).map_err(LanzouError::Io)?;
        Ok(())
    }

    /// 从本地文件加载 VFS。
    ///
    /// 若文件不存在将返回 [`LanzouError::NotFound`]。
    pub fn load_local(file_path: PathBuf) -> Result<Self> {
        if !file_path.exists() {
            return Err(LanzouError::NotFound(format!(
            "{}",
            file_path.display()
        )));
        }
        let data = fs::read_to_string(&file_path).map_err(LanzouError::Io)?;
        Self::from_tsv(&data, file_path)
    }

    /// 把当前树序列化为 TSV 字符串。
    ///
    /// 文件头：`V2|<last_modified>|<count>|<root>|<deeper>`
    /// 行内容：`F|id|pid|base64_name|lanzou_id|time|size|md5|ext|chunks|is_trashed|is_deleted`
    pub fn to_tsv(&self) -> String {
        let mut output = String::new();
        output.push_str(&format!(
            "V2|{}|{}|{}|{}\n",
            self.last_modified,
            self.nodes.len(),
            self.root_lanzou_id,
            self.deeperdir_lanzou_id
        ));
        for node in self.nodes.values() {
            let encoded_name = STANDARD.encode(&node.name);
            output.push_str(&format!(
                "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}\n",
                node.node_type.as_mark(),
                node.id,
                node.pid,
                encoded_name,
                node.lanzou_id,
                node.time,
                node.size,
                node.md5,
                node.ext,
                node.chunks,
                node.is_trashed,
                node.is_deleted
            ));
        }
        output
    }

    /// 解析 TSV 字符串为 VFS。
    pub fn from_tsv(data: &str, file_path: PathBuf) -> Result<Self> {
        let mut lines = data.lines().filter(|l| !l.trim().is_empty());

        let header = lines
            .next()
            .ok_or_else(|| LanzouError::Other("VFS 文件为空".into()))?;
        let h_parts: Vec<&str> = header.split('|').collect();
        if h_parts.len() < 4 || (h_parts[0] != "V1" && h_parts[0] != "V2") {
            return Err(LanzouError::Other("VFS 文件格式不识别".into()));
        }
        let is_v2 = h_parts[0] == "V2";
        let last_modified = h_parts[1].parse::<u64>().unwrap_or(0);
        let root_lanzou_id = h_parts[3].to_string();
        let deeperdir_lanzou_id = h_parts.get(4).unwrap_or(&"").to_string();

        let mut nodes = HashMap::new();
        let mut max_id = 0u64;

        for line in lines {
            let p: Vec<&str> = line.split('|').collect();
            if p.len() < 10 {
                continue;
            }
            let id = p[1].parse::<u64>().unwrap_or(0);
            if id > max_id {
                max_id = id;
            }

            let (name, suf_idx) = if is_v2 {
                // V2 格式：name 为 base64 编码，无管道字符
                let decoded = STANDARD
                    .decode(p[3])
                    .ok()
                    .and_then(|b| String::from_utf8(b).ok())
                    .unwrap_or_else(|| p[3].to_string());
                (decoded, 4usize)
            } else {
                // V1 格式：name 可能含管道，根据尾部标志字段反推
                let len = p.len();
                let last_val = p[len - 1];
                let prev_val = p[len - 2];
                let (has_trashed, has_deleted) = match (prev_val, last_val) {
                    ("true" | "false", "true" | "false") => (true, true),
                    (_, "true" | "false") => (true, false),
                    _ => (false, false),
                };
                let suffix_fields = 6 + (has_trashed as usize) + (has_deleted as usize);
                let suf_idx = len - suffix_fields;
                let name = p[3..suf_idx].join("|");
                (name, suf_idx)
            };

            let node = VfsNode {
                node_type: NodeType::from_mark(p[0]),
                id,
                pid: p[2].parse::<u64>().unwrap_or(0),
                name,
                lanzou_id: p[suf_idx].to_string(),
                time: p[suf_idx + 1].parse::<u64>().unwrap_or(0),
                size: p[suf_idx + 2].to_string(),
                md5: p[suf_idx + 3].to_string(),
                ext: p[suf_idx + 4].to_string(),
                chunks: p[suf_idx + 5].to_string(),
                is_trashed: p.get(suf_idx + 6).copied().unwrap_or("false") == "true",
                is_deleted: p.get(suf_idx + 7).copied().unwrap_or("false") == "true",
            };
            nodes.insert(id, node);
        }

        Ok(Self {
            last_modified,
            root_lanzou_id,
            deeperdir_lanzou_id,
            nodes,
            next_id: max_id + 1,
            file_path,
        })
    }

    /// 更新最后修改时间。
    pub fn touch(&mut self) {
        self.last_modified = current_timestamp_millis();
    }

    /// 列出某父目录下未删除的子节点（按目录优先 + 名称字典序）。
    pub fn list_dir(&self, pid: u64) -> Vec<VfsNode> {
        let mut children: Vec<VfsNode> = self
            .nodes
            .values()
            .filter(|n| n.pid == pid && !n.is_deleted)
            .cloned()
            .collect();
        children.sort_by(|a, b| {
            if a.node_type != b.node_type {
                if a.node_type == NodeType::D {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                }
            } else {
                a.name.cmp(&b.name)
            }
        });
        children
    }

    /// 创建一个目录节点。
    ///
    /// 返回新节点 ID。
    pub fn create_folder(&mut self, pid: u64, name: &str, lanzou_id: &str) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let node = VfsNode {
            node_type: NodeType::D,
            id,
            pid,
            name: name.to_string(),
            lanzou_id: lanzou_id.to_string(),
            time: current_timestamp_millis(),
            size: String::new(),
            md5: String::new(),
            ext: String::new(),
            chunks: String::new(),
            is_trashed: false,
            is_deleted: false,
        };
        self.nodes.insert(id, node);
        self.touch();
        id
    }

    /// 创建一个文件节点。
    ///
    /// `chunks` 为分片数；`size` 是人类可读字符串。
    pub fn add_file(
        &mut self,
        pid: u64,
        name: &str,
        lanzou_id: &str,
        size: &str,
        md5: &str,
        ext: &str,
        chunks: u32,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let chunks_str = if chunks > 1 {
            chunks.to_string()
        } else {
            "1".to_string()
        };
        let node = VfsNode {
            node_type: NodeType::F,
            id,
            pid,
            name: name.to_string(),
            lanzou_id: lanzou_id.to_string(),
            time: current_timestamp_millis(),
            size: size.to_string(),
            md5: md5.to_string(),
            ext: ext.to_string(),
            chunks: chunks_str,
            is_trashed: false,
            is_deleted: false,
        };
        self.nodes.insert(id, node);
        self.touch();
        id
    }

    /// 修改父节点 ID（蓝奏云不支持真正的 move，VFS 自行维护）。
    pub fn move_node(&mut self, id: u64, new_pid: u64) -> Result<()> {
        if id == new_pid {
            return Err(LanzouError::Invalid("不能将目录移入自身".into()));
        }
        if !self.nodes.contains_key(&id) {
            return Err(LanzouError::NotFound(format!("VFS 节点 {id}")));
        }
        if let Some(node) = self.nodes.get_mut(&id) {
            node.pid = new_pid;
            node.time = current_timestamp_millis();
        }
        self.touch();
        Ok(())
    }

    /// 递归墓碑化某节点及其后代（不可恢复删除）。
    pub fn delete_node(&mut self, target_id: u64) {
        let mut to_delete = vec![target_id];
        let mut i = 0;
        while i < to_delete.len() {
            let current = to_delete[i];
            let children: Vec<u64> = self
                .nodes
                .values()
                .filter(|n| n.pid == current)
                .map(|n| n.id)
                .collect();
            for c in children {
                if !to_delete.contains(&c) {
                    to_delete.push(c);
                }
            }
            i += 1;
        }
        let now = current_timestamp_millis();
        for del_id in to_delete {
            if let Some(node) = self.nodes.get_mut(&del_id) {
                node.is_deleted = true;
                node.time = now;
            }
        }
        self.touch();
    }

    /// 把节点移入回收站（仍可恢复）。
    pub fn trash_node(&mut self, target_id: u64) {
        let mut to_trash = vec![target_id];
        let mut i = 0;
        while i < to_trash.len() {
            let current = to_trash[i];
            let children: Vec<u64> = self
                .nodes
                .values()
                .filter(|n| n.pid == current)
                .map(|n| n.id)
                .collect();
            for c in children {
                if !to_trash.contains(&c) {
                    to_trash.push(c);
                }
            }
            i += 1;
        }
        let now = current_timestamp_millis();
        for id in to_trash {
            if let Some(node) = self.nodes.get_mut(&id) {
                node.is_trashed = true;
                node.time = now;
            }
        }
        self.touch();
    }

    /// 计算某节点到根的深度（根 = 0，根的直接子节点 = 1）。
    ///
    /// 若链路出现断层（父节点不存在）则截断到已知最大深度。
    pub fn depth_of(&self, id: u64) -> usize {
        let mut depth = 0;
        let mut cur = id;
        while cur != 0 {
            match self.nodes.get(&cur) {
                Some(n) => {
                    depth += 1;
                    cur = n.pid;
                }
                None => break,
            }
        }
        depth
    }

    /// 把一个远程 VFS 合并进本地。
    ///
    /// 规则：相同 ID 取 `time` 较大者；仅一方存在时保留。
    /// 合并后会做孤儿修复：若父节点不存在但子节点未删除，则把子节点提升到根目录。
    pub fn merge_with(&self, cloud_tree: &VfsTree) -> Self {
        let mut merged: HashMap<u64, VfsNode> = HashMap::new();
        let mut all_ids = std::collections::HashSet::new();
        for id in self.nodes.keys() {
            all_ids.insert(*id);
        }
        for id in cloud_tree.nodes.keys() {
            all_ids.insert(*id);
        }
        for id in all_ids {
            let local = self.nodes.get(&id);
            let cloud = cloud_tree.nodes.get(&id);
            match (local, cloud) {
                (Some(l), Some(c)) => {
                    let keep = if l.time > c.time { l } else { c };
                    merged.insert(id, keep.clone());
                }
                (Some(l), None) => {
                    merged.insert(id, l.clone());
                }
                (None, Some(c)) => {
                    merged.insert(id, c.clone());
                }
                _ => {}
            }
        }
        // 孤儿修复
        for (id, node) in merged.clone().iter() {
            if node.pid != 0 {
                let parent_alive = merged
                    .get(&node.pid)
                    .map(|p| !p.is_deleted)
                    .unwrap_or(false);
                if !parent_alive && !node.is_deleted {
                    if let Some(n) = merged.get_mut(id) {
                        n.pid = 0;
                        n.time = current_timestamp_millis();
                    }
                }
            }
        }

        Self {
            last_modified: std::cmp::max(self.last_modified, cloud_tree.last_modified),
            root_lanzou_id: cloud_tree.root_lanzou_id.clone(),
            deeperdir_lanzou_id: cloud_tree.deeperdir_lanzou_id.clone(),
            nodes: merged,
            next_id: std::cmp::max(self.next_id, cloud_tree.next_id),
            file_path: self.file_path.clone(),
        }
    }

    /// 计算本地路径缓存为空时返回结构体不绑定文件。
    pub fn file_path(&self) -> &Path {
        &self.file_path
    }
}

/// 工具函数：返回当前 Unix 毫秒时间戳。
pub fn current_timestamp_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 蓝奏云原生支持的文件扩展名白名单（蓝奏云对扩展名有强校验，
/// 不在白名单内的会被拒收，此处统一映射为 `.iso`）。
pub fn safe_lanzou_ext(original: &str) -> String {
    const ALLOWED: &[&str] = &[
        "doc", "docx", "zip", "rar", "apk", "txt", "exe", "7z", "e", "z", "ct", "ke", "cetrainer",
        "db", "tar", "pdf", "w3x", "epub", "mobi", "azw", "azw3", "osk", "osz", "xpa", "cpk",
        "lua", "jar", "dmg", "ppt", "pptx", "xls", "xlsx", "mp3", "ipa", "iso", "img", "gho",
        "ttf", "ttc", "txf", "dwg", "bat", "imazingapp", "dll", "crx", "xapk", "conf", "deb",
        "rp", "rpm", "rplib", "mobileconfig", "appimage", "lolgezi", "flac", "cad", "hwt",
        "accdb", "ce", "xmind", "enc", "bds", "bdi", "ssf", "it", "pkg", "cfg", "mp4", "avi",
        "png", "jpeg", "jpg", "gif", "webp", "brushset",
    ];
    let lower = original.to_ascii_lowercase();
    if ALLOWED.contains(&lower.as_str()) {
        lower
    } else {
        "iso".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_tree() -> VfsTree {
        let mut t = VfsTree::new("ROOT".into(), "DEEPER".into(), PathBuf::from("/tmp/test.tsv"));
        let f1 = t.create_folder(0, "docs", "lanzou-1");
        let f2 = t.create_folder(0, "videos", "lanzou-2");
        t.create_folder(f1, "papers", "lanzou-3");
        t.add_file(f2, "intro.mp4", "lanzou-4", "1 MB", "d41d8cd9", "mp4", 1);
        t
    }

    #[test]
    fn round_trip_tsv() {
        let tree = make_tree();
        let serialized = tree.to_tsv();
        let restored =
            VfsTree::from_tsv(&serialized, PathBuf::from("/tmp/test_restore.tsv")).unwrap();
        assert_eq!(restored.nodes.len(), tree.nodes.len());
        assert_eq!(restored.root_lanzou_id, tree.root_lanzou_id);
    }

    #[test]
    fn list_dir_sorts_dirs_first() {
        let tree = make_tree();
        let root = tree.list_dir(0);
        assert_eq!(root.len(), 2);
        assert_eq!(root[0].name, "docs"); // 字典序 + 目录优先
        assert_eq!(root[1].name, "videos");
    }

    #[test]
    fn depth_calculates() {
        let tree = make_tree();
        let papers = tree
            .nodes
            .values()
            .find(|n| n.name == "papers")
            .unwrap();
        assert_eq!(tree.depth_of(papers.id), 2);
    }

    #[test]
    fn safe_lanzou_ext_maps_unknown_to_iso() {
        assert_eq!(safe_lanzou_ext("docx"), "docx");
        assert_eq!(safe_lanzou_ext("unknown"), "iso");
        assert_eq!(safe_lanzou_ext("MP3"), "mp3");
    }
}
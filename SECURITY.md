# 安全说明（Security Policy）

本项目处理蓝奏云账号凭证，因此安全是头等大事。

## 哪些文件包含敏感信息？

| 文件 | 内容 | 是否入版本库 |
|------|------|--------------|
| `data/config.toml` | 默认模板（占位） | ✅ 入库 |
| `data/account.json` | 蓝奏云会话 Cookie (`ylogin` + `phpdisk_info`) | ❌ 已在 `.gitignore` |
| `data/vfs_tree.tsv` | 本地 VFS 状态（含蓝奏云文件夹 ID） | ❌ 已在 `.gitignore` |

## 使用前必须修改

`data/config.toml` 中的 `[lanzou]` 段默认是占位符，**请勿**直接使用仓库里的值。
首次启动前请把：

```toml
[lanzou]
username = "YOUR_PHONE_OR_USERNAME"
password = "YOUR_PASSWORD"
```

替换为你自己的蓝奏云账号密码。

## 登录成功后会发生什么？

1. 蓝奏云会返回两个 Cookie：`ylogin` + `phpdisk_info`
2. 程序把它们写入 `data/account.json`（该文件已被 `.gitignore` 忽略）
3. **同时把 `data/config.toml` 中 `[lanzou].password` 字段自动清空**

这样配置文件里不会长期存留明文密码。

## 报告漏洞

如发现本项目存在安全问题，请私下联系维护者，**不要**直接提交 Issue。
请提供：
- 复现步骤
- 潜在影响
- 可能的修复建议

我们会在确认后尽快修复并发布补丁版本。

## 责任声明

- 本项目为非官方蓝奏云客户端，使用需遵守蓝奏云服务条款
- 维护者不对因账号被封禁、文件泄漏等产生的损失负责
- 请勿将本项目用于商业用途或大规模爬取
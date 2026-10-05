# Lanzou WebDAV

把蓝奏云（[Lanzou Cloud / woozooo](https://up.woozooo.com)）挂载为标准 WebDAV，
让 Infuse、极影视、RaiDrive、Kodi、VLC 等客户端可以直接读取、写入蓝奏云上的文件。

## 特性

- ✅ 纯 Rust 实现，**无需** Node.js / 浏览器，自动绕过阿里 WAF `acw_sc__v2`
- ✅ WebDAV Class 1/2：`OPTIONS / PROPFIND / GET / HEAD / PUT / MKCOL / DELETE`
- ✅ HTTP Range 字节流转发，Infuse 视频秒开、不缓存整段
- ✅ 单文件 / 大文件自动分片上传（蓝奏云单文件 100 MB 上限）
- ✅ MD5 秒传复用：相同 MD5 不重复上传
- ✅ 目录结构 VFS 抽象，支持无限嵌套
- ✅ Docker 一行启动；账号持久化到 `data/account.json`
- ✅ 配置可通过 `data/config.toml` 持久化

## 快速开始

### Docker（推荐）

```bash
docker run -d --name lanzou-webdav \
  -p 8080:8080 \
  -v $(pwd)/data:/data \
  -e RUST_LOG=info \
  ghcr.io/maple/lanzou-webdav:latest
```

挂载的 `data/` 目录会保存：
- `config.toml` —— 监听端口、WebDAV 账号、首次登录的蓝奏云密码
- `account.json` —— 登录后的蓝奏云 Cookie（自动写入，请勿删）
- `vfs_tree.tsv` —— VFS 树状态（重启后保留目录结构）

### 从源码构建并运行

```bash
cargo build --release
mkdir -p ./data
# 编辑 ./data/config.toml，填入蓝奏云账号密码
./target/release/lanzou_webdav
```

服务监听 `0.0.0.0:8080`，WebDAV 根路径为 `/dav`：

```
http://your-ip:8080/dav
```

## 配置文件

`data/config.toml`：

```toml
listen_port = 8080

[webdav]
username = "admin"
password = "admin"

[lanzou]
username = "YOUR_PHONE_OR_USERNAME"
password = "YOUR_PASSWORD"   # 登录成功后会自动清空
```

## SDK 复用

蓝奏云相关代码全部封装在 `crate/lanzou-sdk`，可以在其它 Rust 项目中直接依赖：

```toml
[dependencies]
lanzou-sdk = { git = "https://github.com/MapleLeaf/lanzou-webdav", subdirectory = "crate/lanzou-sdk" }
```

## 目录结构

```
lanzou_webdav/
├── Cargo.toml                  # workspace 根 + binary
├── Dockerfile                  # 多阶段构建（Alpine ~ 30 MB 镜像）
├── crate/
│   └── lanzou-sdk/             # 蓝奏云 SDK
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs
│           ├── client.rs       # 已登录客户端
│           ├── downloader.rs   # 分享直链解析
│           ├── crypto.rs       # WAF + 分片名加密
│           ├── vfs.rs          # 虚拟文件系统
│           ├── account.rs      # 会话持久化
│           ├── model.rs        # 数据模型
│           ├── uploader.rs     # 上传助手
│           └── error.rs        # 统一错误
├── src/                        # WebDAV 服务
│   ├── main.rs
│   ├── config.rs
│   ├── state.rs
│   └── webdav.rs
└── data/                       # 运行时持久化（Docker volume）
    ├── config.toml
    ├── account.json
    └── vfs_tree.tsv
```

## License

MIT
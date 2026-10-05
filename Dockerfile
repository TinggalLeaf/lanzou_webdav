# =============================================================================
# Lanzou WebDAV —— 多阶段 Dockerfile（追求最快 + 最小）
# =============================================================================
# 设计：
#   1. builder 阶段：用 rust:1.88-alpine 编译 Release 二进制
#   2. runtime 阶段：Alpine 3.20 + 仅二进制 + tzdata，几乎 0 攻击面
# =============================================================================

# ----------- 阶段 1：构建 -----------
# Rust 1.88+ 支持 edition2024 与最新 chrono / time / encoding_rs 等依赖
FROM rust:1.88-alpine AS builder

# musl + 静态链接所需工具链
RUN apk add --no-cache musl-dev pkgconfig openssl-dev

WORKDIR /build

# 一次性复制所有源码
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY crate ./crate

# 直接做完整构建（依赖会自动下载并编译）
RUN cargo build --release --locked \
    && strip target/release/lanzou_webdav

# ----------- 阶段 2：运行时（最小镜像） -----------
FROM alpine:3.20

# 仅安装时区与 CA 证书
RUN apk add --no-cache ca-certificates tzdata \
    && addgroup -S lanzou && adduser -S lanzou -G lanzou

# 数据目录（Docker volume 推荐挂载点）
RUN mkdir -p /data && chown -R lanzou:lanzou /data

# 复制二进制
COPY --from=builder /build/target/release/lanzou_webdav /usr/local/bin/lanzou_webdav

# 健康检查
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD wget -qO- http://127.0.0.1:${LANZOU_PORT:-8080}/health || exit 1

USER lanzou
WORKDIR /data

# 数据目录挂载点
VOLUME ["/data"]

# 默认端口（运行时可通过环境变量 LANZOU_PORT 覆盖）
ENV LANZOU_PORT=8080 \
    LANZOU_DATA_DIR=/data \
    RUST_LOG=info,lanzou_sdk=info

EXPOSE ${LANZOU_PORT}

ENTRYPOINT ["/usr/local/bin/lanzou_webdav"]
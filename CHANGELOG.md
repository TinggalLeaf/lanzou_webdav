# Changelog

## [0.1.1](https://github.com/TinggalLeaf/lanzou_webdav/compare/v0.1.0...v0.1.1) (2026-10-10)


### Bug Fixes

* **docker:** 升级基础镜像到 rust:1.86-alpine 以支持 edition2024 依赖 ([c9cc0c7](https://github.com/TinggalLeaf/lanzou_webdav/commit/c9cc0c7152036fa49fa0a1dc8acf227e55218be3))
* **docker:** 升级基础镜像到 rust:1.88-alpine（encoding_rs 等需 rustc 1.88） ([6e21340](https://github.com/TinggalLeaf/lanzou_webdav/commit/6e213405576f1f817e86147d60323e77dddb7dd0))
* **docker:** 移除 stub 预热阶段，直接做完整构建 ([e687944](https://github.com/TinggalLeaf/lanzou_webdav/commit/e687944f51190f4eea105ce6365ae03262b4b46d))
* **sdk:** 重新导出 CHUNK_LIMIT / ProgressCb / UploadResult / Uploader ([dac00f6](https://github.com/TinggalLeaf/lanzou_webdav/commit/dac00f6fd3f73661eee6ca48417478182fef2dab))

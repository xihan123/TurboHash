# TurboHash

<div align="center">

**Rust 文件哈希计算工具**

![TurboHash](https://socialify.git.ci/xihan123/TurboHash/image?description=1&forks=1&issues=1&language=1&name=1&owner=1&pulls=1&stargazers=1&theme=Auto)
[![CI/CD](https://github.com/xihan123/TurboHash/actions/workflows/build-release.yml/badge.svg)](https://github.com/xihan123/TurboHash/actions/workflows/build-release.yml)
[![Release](https://img.shields.io/github/v/release/xihan123/TurboHash)](https://github.com/xihan123/TurboHash/releases)
[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)

[下载](#下载) • [快速开始](#快速开始) • [编译](#从源码编译) • [配置](#配置)

</div>

---

## 简介

TurboHash 计算四种哈希值（CRC32、MD5、SHA1、xxHash3）在单次遍历中。通过 SIMD 硬件加速、自适应 I/O 和多核并行处理，在 SSD 上达到 1-2 GB/s 的吞吐量。二进制文件优化后小于 5MB，无需外部依赖。

所有错误通过 `Result` 类型处理，不会崩溃。缓存使用 SQLite 存储，xxhash3 作为校验键，缓存命中时接近瞬时返回（< 1ms）。界面使用系统字体显示中文，支持原生文件对话框。

---

## 截图

<!-- TODO: 添加界面截图 -->

---

## 下载

从 [GitHub Releases](https://github.com/xihan123/TurboHash/releases/latest) 获取最新版本。

| 平台 | 文件名 |
|------|--------|
| Windows x64 | `TurboHash-windows-x64.exe` |
| macOS Intel | `TurboHash-macos-x64` |
| macOS Apple Silicon | `TurboHash-macos-arm64` |
| Linux x64 | `TurboHash-linux-x64` |

所有平台二进制文件均约 5MB，独立运行无需依赖。

---

## 快速开始

### Windows

```powershell
TurboHash-windows-x64.exe
# 或带参数运行
TurboHash-windows-x64.exe path\to\file.txt path\to\folder
```

### macOS / Linux

```bash
chmod +x TurboHash-*
./TurboHash-* [文件/文件夹路径...]
```

### 使用方法

1. 拖放文件/文件夹或点击按钮添加
2. 添加后 500ms 自动开始计算
3. 实时显示三种哈希值
4. 结果自动缓存，再次计算直接读取

---

## 从源码编译

### 前置要求

- Rust 1.85+（edition 2024）
- Git

### 编译

```bash
git clone https://github.com/xihan123/TurboHash.git
cd TurboHash
cargo build --release
cargo run -- path/to/file.txt
```

### 跨平台编译

```bash
# Windows x64
cargo build --release --target x86_64-pc-windows-msvc

# macOS Intel
cargo build --release --target x86_64-apple-darwin

# macOS Apple Silicon
cargo build --release --target aarch64-apple-darwin

# Linux x64
cargo build --release --target x86_64-unknown-linux-gnu
```

---

## 配置

配置存储在可执行文件同目录的 `hash_cache.db`。通过 **设置 → 缓存配置** 修改：

- **最小文件大小**：小于此值的文件不缓存（默认 1MB）
- **保留天数**：删除超过此时间的缓存（默认 30 天）
- **缓冲区大小**：中等文件的 I/O 缓冲区，64KB - 512MB 范围（默认 256KB）
- **MMAP 块大小**：大文件的内存映射块大小（默认 4MB）

### 自适应 I/O 策略

TurboHash 根据文件大小选择不同 I/O 方式：

- **小于 64KB**：单次 `read()` 调用读取完整文件
- **64KB - 512MB**：`BufReader` 分块读取
- **大于 512MB**：内存映射文件，按配置的块大小处理

---

## 开发

### 测试

```bash
cargo test
cargo test -- --nocapture
cargo test hash::tests      # 哈希算法验证
cargo test engine::tests    # I/O 引擎测试
```

### 项目结构

```
src/
├── main.rs      # 入口、CLI 参数、GUI 初始化
├── error.rs     # 错误类型定义
├── hash.rs      # 四种哈希算法的单遍计算
├── engine.rs    # 自适应 I/O 引擎
├── worker.rs    # Rayon 并行处理
├── cache.rs     # SQLite 缓存
├── ui.rs        # egui 界面逻辑
└── font.rs      # 系统字体加载
```

---

## 架构

### 数据流

```
用户添加文件（拖放/对话框）
    ↓
TurboHashApp::add_files() [ui.rs]
    ↓
500ms 后触发自动计算
    ↓
生成 WorkerThread [worker.rs]
    ↓
Rayon 并行迭代器处理文件
    ↓
对每个文件：
    1. compute_xxhash3_only() → Xxhash3Computed 消息
    2. UI 通过 HashCache::get_by_path() 检查缓存 [cache.rs]
    3. 如果缓存命中（xxhash3 匹配）→ 标记 Completed（from_cache=true）
    4. 如果缓存未命中 → compute_file_hash() → FileCompleted 消息
    5. UI 保存结果到缓存
    ↓
通过 UiMessage 通道实时更新进度
```

### 设计要点

- 所有函数返回 `HashResult<T>`，不使用 `unwrap()` 或 `expect()`
- 使用 `crossbeam-channel` 实现 Actor 风格并发
- 两阶段计算：先计算 xxhash3 校验缓存，再计算其余哈希
- xxhash3 作为缓存命中/未命中检测键

---

## 致谢

主要依赖：

- [eframe/egui](https://github.com/emilk/egui) - 即时模式 GUI
- [rayon](https://github.com/rayon-rs/rayon) - 并行处理
- [rusqlite](https://github.com/rusqlite/rusqlite) - SQLite 绑定
- [ring](https://github.com/briansmith/ring) - 加密库（SHA1）
- [crc32fast](https://github.com/srijs/rust-crc32fast) - 硬件加速 CRC32
- [xxhash-rust](https://github.com/Cyan4973/xxHash) - xxHash3

---

<div align="center">

**如果这个项目对您有帮助，请给一个 Star**

[⬆ 返回顶部](#turbohash)

</div>

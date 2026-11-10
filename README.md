# macrdp

[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)
[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)
[![macOS](https://img.shields.io/badge/macOS-14%2B-black.svg)](https://www.apple.com/macos/)
[![Apple Silicon](https://img.shields.io/badge/Apple%20Silicon-Supported-green.svg)](#)

**[English](README_EN.md)** | 中文

> **Fork 说明**：本项目是 [x6nux/macrdp](https://github.com/x6nux/macrdp) 的个人 fork，按自己的长期远程使用需求做了较多修改（魔改版）。上游提供了 macOS RDP 协议栈、ScreenCaptureKit 采集、VideoToolbox 编码和 Tauri 桌面端的基础实现；本 fork 的重点是稳定性、远程输入/显示体验以及本地构建与分发。具体改动见 [本 fork 的改动](#本-fork-的改动)。

**macOS 远程桌面服务端**

原生 macOS RDP 服务端。从 Windows、Linux、iOS 或 Android 远程连接你的 Mac — 支持任何标准 RDP 客户端，如 Windows 远程桌面 (mstsc)、Microsoft Remote Desktop、FreeRDP。

> **为什么选 macrdp？** macOS 没有内置 RDP 服务端，自带的 VNC 又慢又模糊。macrdp 让你的 Mac 拥有一流的远程桌面体验 — 快速、清晰、开箱即用兼容所有 RDP 客户端。

---

## 功能特性

- **标准 RDP 协议** — 兼容任何 RDP 客户端，客户端无需安装特殊软件
- **硬件加速编码** — 通过 Apple VideoToolbox GPU 加速 H.264，Apple Silicon 上低延迟
- **高保真色彩** — AVC444 模式像素级色彩还原（RDP 10）
- **完整键鼠支持** — 104 键映射、数字键盘、修饰键、滚轮，完整输入注入
- **HiDPI / Retina 支持** — 2x/3x 倍率采集，远程 4K 高清显示
- **灵活配置** — 分辨率、帧率、码率、编码器、质量预设，简单 TOML 配置
- **安全连接** — NLA/CredSSP 认证 + 自动生成 TLS 证书
- **锁屏采集** — 锁屏时自动切换 CoreGraphics 回退

---

## 本 fork 的改动

本 fork 面向长期无人值守的远程使用（以 Windows 客户端为主），在上游基础上主要做了以下修改（截至 v0.1.11）：

### 稳定性

- 修复启动 ScreenCaptureKit 时的 SIGSEGV：`SCStreamConfiguration` 的颜色属性改为内存拷贝，不再由 `assign` 持有，避免悬垂的 CFString。
- 显示器休眠不再中断远程会话：连接时唤醒显示器；会话期间显示器再次休眠时保持采集，唤醒后自动恢复。
- 未授予屏幕录制权限时，服务不再卡在 “starting”，而是给出明确错误并退出；启动流程也不会被 TCC 授权弹窗阻塞。
- TLS 证书写入可写目录，不再依赖启动时的当前工作目录。

### 显示

- **分辨率策略**：`auto` 跟随客户端窗口，与 Windows 远程桌面的行为一致（按客户端请求的 Bitmap-capset 桌面尺寸调整采集，上限为显示器物理分辨率）；指定 `WxH` 时则固定该分辨率，客户端不能覆盖。Windows 在 150% 缩放下全屏连接时只会请求 2560×1440 之类的逻辑尺寸，mstsc 按 1:1 显示在 3840×2160 窗口中就会四周黑边——固定为 3840×2160 即可铺满且清晰。

### 输入

- **可配置的修饰键映射**（`[keyboard] modifier_mapping`）：默认 `shortcut` 按 Windows 习惯把 Ctrl 映射为 Command ⌘、Alt 映射为 Option ⌥、Win 映射为 Control，Ctrl+C/V/X、Ctrl+Delete 可直接使用；`position` 保留按物理位置的 Ctrl→Control、Alt→⌘、Win→⌥。
- 远程 Caps Lock 可切换 macOS 输入法（Text Input Sources），用于中文输入场景。
- 修复按 Caps Lock 导致服务崩溃：HIToolbox 的 TIS 接口要求在主线程调用，现在统一派发到主队列执行，并缓存输入法可用状态。

### 桌面端（Tauri）

- 启动后驻留菜单栏，需要时再打开主窗口；不再常驻 Dock 图标，修正托盘图标显示。
- 主界面显示本机所有 IPv4 地址（局域网 / VPN / 其他），可一键复制；VPN 重连换地址后会自动刷新。
- 修正启动/停止按钮的状态与反馈；权限页可直接跳转系统设置。
- 新增“保持屏幕常亮”开关（默认关闭）。

### 构建与发布

- 支持仅安装 Command Line Tools、没有完整 Xcode 的本地构建。
- 固定自签名证书 `macrdp-dev`：`make sign-setup` 一次创建，之后每次构建都用同一身份签名，重装/升级不再丢失屏幕录制与辅助功能授权（ad-hoc 签名每次构建都会变，系统会当成新 App）。
- 新增 `make app`、`make dmg` 等目标；dmg 改用 `hdiutil` 打包，不再依赖 Finder/AppleScript，SSH 等无图形会话下也能构建。
- CI 迁移到 macOS 26 runner，仅发布 Apple Silicon 包，推送 `v*` tag 自动发布。

### 兼容性

- 配置格式与上游保持兼容，新增配置项都有默认值，旧配置无需修改。
- RDP 协议行为保持与上游一致，仍兼容 mstsc、Microsoft Remote Desktop、FreeRDP 等标准客户端。

---

## 环境要求

- **macOS 14+**（Sonoma 或更高版本，ScreenCaptureKit 所需）
- **Rust 1.75+**
- **Xcode Command Line Tools**（无需完整 Xcode）
  - 本项目不编译 Swift，只链接系统 Swift 运行时；用 Makefile 构建时会自动把 CLT 的 Swift 运行时目录加入 `RUSTFLAGS`
  - 手动 `cargo build` 时如遇 `libswiftCore.dylib` / `swiftCompatibility` 相关链接错误，设置：
    ```bash
    export RUSTFLAGS="-L $(xcode-select -p)/usr/lib/swift/macosx"
    ```
  - 如果本机安装了完整 Xcode 且 `xcode-select -p` 指向它，则无需上面的设置
- 屏幕录制权限（系统设置 > 隐私与安全性）
- 辅助功能权限（用于键盘鼠标注入）

---

## 快速开始

```bash
# 编译
cargo build --release

# 运行
cargo run --release --bin macrdp-server

# 从任意 RDP 客户端连接 → Mac-IP:3389
```

### 构建桌面端

```bash
make install      # 首次：安装前端依赖
make sign-setup   # 首次：创建固定签名证书（推荐）
make app          # 编译 .app 并用固定证书签名
make dmg          # 用 hdiutil 打包 dmg
```

如果不想使用固定签名，可以 `make app-adhoc && make dmg`；但 ad-hoc 签名每次构建都会变化，重新安装后需要重新授予屏幕录制和辅助功能权限。

---

## 配置

复制 `config.example.toml` 为 `config.toml` 并按需修改：

```toml
# 网络
port = 13389

# 认证
username = "admin"
password = "123456"

# 显示
width = 0          # 0 = 自动检测
height = 0
frame_rate = 60
hidpi_scale = 2    # Retina 上 2 倍缩放获得 4K

# 编码
quality = "high_quality"    # low_latency / balanced / high_quality
encoder = "hardware"        # hardware (GPU) / software (CPU)
chroma_mode = "avc420"      # avc420 (兼容) / avc444 (最佳画质)
bitrate_mbps = 50           # 目标码率 (Mbps)

# 日志
log_level = "info"          # trace / debug / info / warn / error
```

配置搜索顺序:
1. `./config.toml`
2. `~/.config/macrdp/config.toml`
3. `~/Library/Application Support/macrdp/config.toml`

---

## 项目结构

```
crates/
├── macrdp-server/       主服务端程序
├── macrdp-capture/      屏幕采集
├── macrdp-input/        键鼠注入
├── macrdp-encode/       视频编码
├── ironrdp-server-gfx/  RDP 协议层 (IronRDP fork)
└── ironrdp-acceptor-patched/
                         RDP 连接接受器
```

---

## 致谢

本项目的诞生离不开以下优秀的开源项目，在此致以诚挚的敬意：

- **[x6nux/macrdp](https://github.com/x6nux/macrdp)** — 本 fork 的上游项目，提供了 macOS RDP 服务端、ScreenCaptureKit 采集、VideoToolbox 编码和 Tauri 桌面端的基础实现。
- **[IronRDP](https://github.com/Devolutions/IronRDP)** — 纯 Rust RDP 协议实现。macrdp 的协议栈基于 ironrdp-server 的 fork，添加了 GFX/AVC444 扩展。
- **[FreeRDP](https://github.com/FreeRDP/FreeRDP)** — 开源 RDP 参考实现。其 AVC444 双流编码方案和 YUV444 B 区域拆分算法是 macrdp 实现的重要参考。
- **[RustDesk](https://github.com/rustdesk/rustdesk)** — 使用 Rust 编写的开源远程桌面软件。其跨平台屏幕采集和输入注入的架构思路给予了很大启发。

---

## 许可证

本项目采用 **GNU 通用公共许可证 v3.0** 授权 — 详见 [LICENSE](LICENSE)。任何基于本项目的衍生作品必须同样以 GPLv3 开源。

本 fork 与上游采用相同许可，并保留上游的版权与许可声明；重新分发时请一并保留。

---

<details>
<summary><b>关键词</b></summary>

macOS RDP 服务端, Mac 远程桌面, Mac 远程桌面服务端, 远程桌面协议 macOS, 从 Windows 连接 Mac, 从 Linux 连接 Mac, 从安卓连接 Mac, 远程控制 Mac, Mac 远程访问, Mac 屏幕共享, Apple Silicon 远程桌面, VNC 替代方案, macOS 远程桌面方案, macOS RDP server, Mac remote desktop server, RDP server for Mac, connect to Mac from Windows, remote control Mac, VNC alternative Mac

</details>

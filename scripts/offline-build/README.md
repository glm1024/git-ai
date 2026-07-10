# Git AI 离线构建脚本

这些脚本用于从检出的源码树构建完整的 Apple Silicon macOS、Linux 和 Windows 离线发行版。它们兼容 POSIX `sh`，因此如果 macOS 上尚未设置可执行权限，请显式使用 `sh` 运行。

## 构建单个产物

```sh
sh scripts/offline-build/build-macos-arm64.sh
sh scripts/offline-build/build-linux-arm64.sh
sh scripts/offline-build/build-linux-x64.sh
sh scripts/offline-build/build-windows-x64.sh
sh scripts/offline-build/build-vscode.sh
sh scripts/offline-build/build-jetbrains.sh
sh scripts/offline-build/package-offline-dist.sh
```

## 构建完整包

```sh
sh scripts/offline-build/build-all.sh
```

默认输出目录为 `offline-dist/git-ai-offline-v<CLI version>/`。
同版本离线包默认由新构建结果替换。

## 首次在线构建与后续离线构建

首次构建需要访问以下上游依赖：

- 用于 Linux 和 Windows 构建器的 Docker 基础镜像及 Debian 软件包。
- Cargo crates 以及 Rust `1.93.0` 目标标准库。
- 在 Apple Silicon macOS 上，需要 Xcode 命令行工具以及通过 `rustup` 安装的 Rust `1.93.0` 工具链。
- 由 `cargo-xwin` 下载的 Microsoft Windows SDK 和 CRT。
- 用于 VS Code 插件的 npm 包以及用于 JetBrains 插件的 Gradle 依赖。

脚本会将 Cargo、xwin、npm 和 Gradle 的依赖缓存到 `build/offline-build/cache/` 目录下。如果后续需要仅使用缓存进行构建，请设置：

```sh
GIT_AI_BUILD_OFFLINE=1 sh scripts/offline-build/build-all.sh
```

对于内网发布流程，请将两个 Builder 镜像同步到内部镜像库，并配置 `GIT_AI_LINUX_BUILDER_IMAGE` 和 `GIT_AI_WINDOWS_BUILDER_IMAGE` 指向这些内部镜像。同时请保留缓存目录，或者将 Cargo/npm/Gradle 的公共注册表替换为经过批准的内部镜像源。

## 输出内容

打包步骤会生成：

- 原生 Apple Silicon macOS 二进制文件。
- Linux x64 和 ARM64 musl 二进制文件。
- Windows x64 MSVC 可执行文件。
- VS Code/Cursor VSIX 和 JetBrains ZIP 插件包。
- 最新的 `SHA256SUMS`、包含匹配内置二进制哈希的重新生成的安装脚本、`INSTALL.md` 以及 `BUILD-METADATA.txt`。

Windows 可执行文件是在本地交叉编译的。在分发之前，请在真实的 Windows x64 机器上运行 Windows 安装、hook 设置以及 commit 归因冒烟测试。

macOS 脚本故意限制为仅在 Apple Silicon macOS 上运行。它会生成 `git-ai-macos-arm64`；此离线包中不包含 Intel macOS 二进制文件。

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

每个构建脚本都会在产物旁写入 `.build-metadata` 来源文件，记录源码 commit、
构建时源码是否干净以及产物 SHA-256。`package-offline-dist.sh` 只接受由当前
commit 的干净源码构建、且哈希仍匹配的全套产物；缺失来源文件、混用不同
commit、从脏源码构建或构建后被替换的产物都会直接拒绝打包。源码更新后必须
重新构建全部六个产物，不能只复用 `build/offline-build/artifacts/` 中的旧文件。
来源文件会随产物一起进入最终离线包，便于发布后核验每个文件的构建来源。

这是一道未签名的本地一致性门禁，用来避免误用旧产物、脏源码产物或混合
commit 产物；它不是 SLSA/in-toto attestation，也不提供加密签名或供应链身份
证明。

只运行来源门禁的 POSIX `sh` 回归测试（不会构建或打包）：

```sh
sh scripts/offline-build/test-source-metadata.sh
```

只运行离线安装器的事务回滚与故障注入回归（不会构建、打包或安装到真实用户目录）：

```sh
sh scripts/offline-build/test-install-rollback.sh
```

该测试会在临时 `HOME` 中执行 Unix 安装模板和当前离线包脚本，覆盖旧二进制、
已有 `git` shim、CLI 链接的成组升级/恢复以及首次失败保留配置、SQLite 和 outbox；
Windows 脚本在没有 PowerShell 的平台只做事务结构静态门禁，正式分发前仍需在真实
Windows x64 上执行安装失败与升级回滚测试。

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

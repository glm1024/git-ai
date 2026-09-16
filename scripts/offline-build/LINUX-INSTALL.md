# Git AI Linux 离线安装

本包同时包含 x86_64 和 ARM64 两种 CLI。安装脚本会读取 Linux 的系统与 CPU
架构，自动选择对应文件并在安装前完成内部完整性校验；用户无需手动执行
`sha256sum`，也不要手动指定二进制路径。

请使用开发者本人的 Linux 账号安装，不要使用 `root` 或 `sudo`。多人共用服务器
时，每个人都应使用独立账号、HOME 和项目目录。

解压整个压缩包，进入解压后的目录执行：

```sh
bash ./install.sh
git-ai --version
```

安装脚本支持：

- `uname -m` 为 `x86_64`：安装 x64 CLI。
- `uname -m` 为 `aarch64` 或 `arm64`：安装 ARM64 CLI。

其他操作系统或 CPU 架构会停止安装。安装成功后，请关闭并重新打开终端及
VS Code Remote SSH 窗口，再核对 `git-ai --version`。VS Code 插件不在本包内，
需要在对应 Remote SSH 窗口中单独从 VSIX 安装。

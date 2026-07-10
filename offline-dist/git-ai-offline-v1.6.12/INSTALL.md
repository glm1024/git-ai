# Git AI 内网离线安装手册

版本：`v1.6.12`

适用范围：研发内网 Linux、Windows、Apple Silicon macOS，以及 VS Code、Cursor、JetBrains IDE。

本手册假定安装机器不能访问 GitHub。CLI 安装脚本通过 `GIT_AI_LOCAL_BINARY` 使用离线包内的预编译二进制，不会下载外网文件。

## 包内容

```text
git-ai-offline-v1.6.12/
  SHA256SUMS
  INSTALL.md
  install.sh
  install.ps1
  linux/
    git-ai-linux-x64
    git-ai-linux-arm64
  macos/
    git-ai-macos-arm64
  windows/
    git-ai-windows-x64.exe
  vscode/
    git-ai.git-ai-vscode-0.1.22.vsix
  jetbrains/
    Git_AI-0.1.12.zip
```

当前离线包支持：

- Linux `x86_64`：`linux/git-ai-linux-x64`
- Linux `aarch64` / `arm64`：`linux/git-ai-linux-arm64`
- macOS Apple Silicon `arm64`：`macos/git-ai-macos-arm64`
- Windows `AMD64` / `x64`：`windows/git-ai-windows-x64.exe`
- VS Code 和 Cursor：`vscode/git-ai.git-ai-vscode-0.1.22.vsix`
- IntelliJ IDEA、PyCharm、WebStorm 等 JetBrains IDE：`jetbrains/Git_AI-0.1.12.zip`



## 安装前注意事项

- CLI 用普通用户安装，不要使用 `root`、`sudo` 或 Windows 管理员 PowerShell。
- 多人共用 Linux 研发机时，尽量一人一个 OS 用户，并设置各自的 `git config user.name` 和 `git config user.email`。多人共用同一个系统账号会让本地状态混在同一个 `$HOME/.git-ai` 下。
- 安装前已经产生的代码不会被追溯成 AI 生成代码。安装完成后需要重启 Codex、VS Code、Cursor 或 JetBrains IDE。
- CLI 是统计和 hook 的基础组件。VS Code、Cursor、JetBrains 插件是编辑器内展示和采集能力，按实际使用的 IDE 安装。
- 安装脚本会执行 `git-ai install-hooks`，用于配置支持的 Agent/IDE hook；如果后续新增或更换 Agent，可再次执行该命令。



## Linux 安装



### 1. 进入离线包并确认架构

```bash
cd /path/to/git-ai-offline-v1.6.12
uname -m
```

`x86_64` 使用 `linux/git-ai-linux-x64`；`aarch64` 或 `arm64` 使用 `linux/git-ai-linux-arm64`。

### 2. 安装 x64 或 ARM64

Linux x64：

```bash
chmod +x install.sh linux/git-ai-linux-x64
GIT_AI_LOCAL_BINARY="$PWD/linux/git-ai-linux-x64" bash ./install.sh
```

Linux ARM64：

```bash
chmod +x install.sh linux/git-ai-linux-arm64
GIT_AI_LOCAL_BINARY="$PWD/linux/git-ai-linux-arm64" bash ./install.sh
```

安装脚本会把 CLI 安装到 `$HOME/.git-ai/bin`，并尝试把该目录加入当前用户的 shell 配置。当前终端没有立即生效时执行：

```bash
export PATH="$HOME/.git-ai/bin:$PATH"
```



### 3. 配置和验证 CLI

```bash
git ai --version
git ai config set disable_auto_updates true
git ai config set disable_version_checks true
git ai config set telemetry_oss off
git ai install-hooks --verbose
```



## macOS Apple Silicon 安装

仅支持 Apple Silicon（`arm64`）Mac；Intel Mac 不适用本离线包。

```bash
cd /path/to/git-ai-offline-v1.6.12
uname -m
chmod +x install.sh macos/git-ai-macos-arm64
GIT_AI_LOCAL_BINARY="$PWD/macos/git-ai-macos-arm64" bash ./install.sh
```

安装后重新打开终端，并按 Linux 章节的“配置和验证 CLI”执行验证与 hook 配置。

## Windows 安装



### 1. 确认架构和目录

用普通用户 PowerShell 进入离线包目录：

```powershell
cd C:\path\to\git-ai-offline-v1.6.12
$env:PROCESSOR_ARCHITECTURE
```



### 2. 安装 CLI

```powershell
$env:GIT_AI_LOCAL_BINARY = (Resolve-Path .\windows\git-ai-windows-x64.exe).Path
powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1
```

安装脚本会把 CLI 安装到 `$HOME\.git-ai\bin`，并尝试写入当前用户 PATH。当前 PowerShell 没有立即生效时执行：

```powershell
$env:Path = "$HOME\.git-ai\bin;$env:Path"
```



### 3. 配置和验证 CLI

```powershell
git ai --version
git ai config set disable_auto_updates true
git ai config set disable_version_checks true
git ai config set telemetry_oss off
git ai install-hooks --verbose
```



## IDE 插件安装

先完成对应平台的 CLI 安装，再安装 IDE 插件。插件安装不替代 CLI。

### VS Code

安装 vscode 插件

### JetBrains IDE

JetBrains 插件要求先安装并能在 PATH 中找到 Git AI CLI。安装后在 IDE 内置 Terminal 验证：

```bash
git ai --version
git ai install-hooks --verbose
```



## Codex、Agent 和 IDE 的使用关系


| 使用场景                     | 必需组件                                |
| ------------------------ | ----------------------------------- |
| Codex 生成代码并统计            | Git AI CLI + `git ai install-hooks` |
| VS Code 编辑并显示 AI 行       | Git AI CLI + VS Code VSIX           |
| Cursor 编辑并显示 AI 行        | Git AI CLI + Cursor VSIX            |
| JetBrains IDE 编辑并显示 AI 行 | Git AI CLI + JetBrains ZIP 插件       |
| 只查看 Git 统计和 blame        | Git AI CLI                          |


安装完成后，无论从 IDE、内置 Terminal 还是外部终端提交，都应使用同一用户的 Git 工作区和 Git 身份配置。

## 功能验收

安装后新开 Codex/IDE 会话，在一个临时 Git 仓库中让 Codex 或其他 AI Agent 修改一小段代码，再执行：

```bash
git ai status --json
git add .
git commit -m "test git ai attribution"
git ai stats HEAD --json
git log --show-notes=ai -1
git ai blame path/to/changed-file
```

验收重点：

- `git ai status --json` 在提交前能看到 checkpoint 或统计信息。
- `git ai stats HEAD --json` 能看到 `ai_additions` 或 `tool_model_breakdown`。
- `git log --show-notes=ai -1` 能看到 AI Git Notes 相关内容。
- `git ai blame` 能在变更文件行级显示 AI / human 来源。
- VS Code、Cursor 或 JetBrains IDE 中能看到对应的 AI 行级展示时，说明 CLI 与 IDE 插件均已生效。

注意：只安装 CLI 后，命令行统计仍可用；没有安装对应 IDE 插件时，不应以编辑器内没有行级展示作为 CLI 安装失败的判断依据。

## 回滚

先关闭 Codex、Agent 和 IDE，再移除 hook：

```bash
git ai uninstall-hooks
```

Linux 删除本地安装：

```bash
rm -rf "$HOME/.git-ai"
```

Windows PowerShell 删除本地安装：

```powershell
Remove-Item -Recurse -Force "$HOME\.git-ai"
```

如果 shell 配置或用户 PATH 中有安装脚本追加的 Git AI 路径，可以手工删除：

```text
# Added by git-ai installer ...
```

IDE 插件需要分别在 VS Code/Cursor 的 Extensions 面板或 JetBrains 的 Plugins 页面卸载。

## 常见问题

- `git ai` 找不到：重新打开终端，或手工把 `$HOME/.git-ai/bin` / `%USERPROFILE%\.git-ai\bin` 放到 PATH 前面。
- 安装脚本提示下载 GitHub：确认设置了 `GIT_AI_LOCAL_BINARY`，并且变量指向离线包内与当前架构匹配的二进制。
- `git ai status` 没有 checkpoint：确认已经执行 `git ai install-hooks --verbose`，并重启 Codex/IDE 后重新开始一次编辑动作。
- IDE 已安装插件但没有行级展示：先确认 CLI 可执行，再确认 IDE 已重启，并确认当前打开的是 Git 仓库内的文件。
- Windows 上出现 daemon lock 或后台服务异常：先关闭 IDE/Agent，结束残留 `git-ai` 进程，再执行 `git ai install-hooks --verbose` 并重启 IDE/Agent。
- 多人共用 Linux 账号时统计归属混乱：为每个开发者分配独立 OS 用户和独立 `$HOME`，不要只依赖 Git 提交作者名区分本地 Git AI 状态。


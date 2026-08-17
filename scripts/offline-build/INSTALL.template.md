# Git AI 离线安装包 v__OFFLINE_VERSION__

本目录是一个不可拆分的发布单元。复制、安装前先校验 `SHA256SUMS`；校验范围包含二进制、插件、构建来源元数据、安装脚本和本手册。任何一项缺失或校验失败都停止安装并重新获取完整包。

## 1. 校验完整性

macOS：

```bash
shasum -a 256 -c SHA256SUMS
```

Linux（任选系统已有的一种工具）：

```bash
sha256sum -c SHA256SUMS
# 或：shasum -a 256 -c SHA256SUMS
```

Windows PowerShell：

```powershell
$failed = $false
Get-Content .\SHA256SUMS | ForEach-Object {
  if ($_ -notmatch '^([0-9a-fA-F]{64})  (.+)$') { throw "Invalid SHA256SUMS line: $_" }
  $expected = $Matches[1].ToLowerInvariant()
  $path = $Matches[2]
  if (-not (Test-Path -LiteralPath $path)) { Write-Error "Missing: $path"; $failed = $true; return }
  $actual = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
  if ($actual -ne $expected) { Write-Error "Checksum mismatch: $path"; $failed = $true }
}
if ($failed) { throw 'Offline bundle verification failed' }
```

同时核对 `BUILD-METADATA.txt`：`cli_version` 应为 `__OFFLINE_VERSION__`，`source_dirty=false`，所有 `.build-metadata` 的 `source_commit` 应与它一致。

## 2. 安装 CLI

不要使用旧版本离线包的安装脚本覆盖新版本。安装器会保留本地配置和统计数据库，并在失败或下次运行时按持久化事务日志恢复可执行文件。

macOS ARM64：

```bash
GIT_AI_LOCAL_BINARY="$PWD/macos/git-ai-macos-arm64" bash ./install.sh
```

Linux x64：

```bash
GIT_AI_LOCAL_BINARY="$PWD/linux/git-ai-linux-x64" bash ./install.sh
```

Linux ARM64：

```bash
GIT_AI_LOCAL_BINARY="$PWD/linux/git-ai-linux-arm64" bash ./install.sh
```

Windows x64 PowerShell：

```powershell
$env:GIT_AI_LOCAL_BINARY = (Resolve-Path .\windows\git-ai-windows-x64.exe).Path
& .\install.ps1
Remove-Item Env:GIT_AI_LOCAL_BINARY
```

关闭并重新打开终端后确认：

```bash
git-ai --version
```

输出版本必须是 `__OFFLINE_VERSION__`。

## 3. CLI-only 必须先下发统计身份

在启动 `git-ai bg`、打开 IDE 或产生待统计 Git/Agent 活动之前，由开发者本人操作系统账号写入 Git AI 统计配置。项目身份仍由 Git remote 自动识别；人员架构只以本配置为准。先创建仅本人可读的 `reporting-profile.json`：

```json
{
  "metrics_api_base_url": "https://<内网统计地址>",
  "reporting_profile": {
    "department_name": "<部门>",
    "office_name": "<处室>",
    "team_name": "<团队，可省略>",
    "user_name": "<姓名>",
    "user_email": "<企业邮箱>"
  }
}
```

macOS/Linux：

```bash
git-ai config reporting-profile set --stdin < reporting-profile.json
git-ai config reporting-profile
git-ai bg start
```

Windows PowerShell：

```powershell
Get-Content -Raw .\reporting-profile.json | git-ai config reporting-profile set --stdin
git-ai config reporting-profile
git-ai bg start
```

显示内容必须与下发信息一致。完成后安全删除包含人员信息的临时 JSON 文件。未配置专用统计地址和完整人员信息时，不得把本机事实视为已进入正式统计链路。

## 4. 安装 IDE 插件

- VS Code：安装 `vscode/__VSCODE_VSIX__`，重启 VS Code 后核对插件版本。
- JetBrains：从磁盘安装 `jetbrains/__JETBRAINS_ZIP__`，重启 IDE 后核对插件版本。

插件与 CLI 必须来自同一个已校验离线包，不要混用其他目录中的同名产物。

## 5. 升级、失败恢复与回滚

- 升级前备份整个 `~/.git-ai`（Windows 为 `%USERPROFILE%\.git-ai`），保留数据库、outbox、配置和升级日志。
- 安装器只回滚当前安装事务中的可执行文件；统计数据库 schema 是只向前迁移的，不会自动降级。
- 安装器默认拒绝从更高 CLI 版本降到更低版本。只有在完成全量备份并验证目标版本能读取当前 schema 后，才可由发布负责人显式设置 `GIT_AI_ALLOW_SCHEMA_UNSAFE_DOWNGRADE=1`；不要把它作为常规回滚开关。
- 进程被强制终止或主机断电后，重新运行同一新版本安装器。它会先读取 `install-transaction` 日志；只有旧入口/备份状态能够唯一证明时才自动恢复或完成上次事务。日志声明旧入口存在、但对应备份缺失时，新旧内容无法区分，安装器会停止并保留现场，必须由发布负责人核验后处理。
- Windows 自更新只有在新二进制版本与预期版本完全一致且写入升级 receipt 后才记为成功；仅看到后台 PowerShell 已启动不代表升级完成。

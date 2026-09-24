# Git AI Windows x64 离线安装

解压整个 ZIP，在普通用户 PowerShell 中进入解压后的目录。

推荐使用包内的 `install.cmd`。它会启动一个独立的 Windows PowerShell
进程，并仅对该进程使用 `ExecutionPolicy Bypass`，因此可兼容常见的本机
`Restricted` / `RemoteSigned` 配置，而不会修改系统或当前用户的持久执行策略。

首次安装或直接使用包内脚本升级：

```powershell
& .\install.cmd
git-ai --version
```

如果已经把可复用的安装器保存到固定位置，建议同时保存 `install.cmd` 和
`install.ps1`：

```powershell
$env:GIT_AI_LOCAL_BINARY = (Resolve-Path -LiteralPath .\windows\git-ai-windows-x64.exe).Path
& 'C:\git-ai-installer\install.cmd'
Remove-Item Env:GIT_AI_LOCAL_BINARY
git-ai --version
```

也可以直接启动 PowerShell 安装脚本：

```powershell
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File .\install.ps1
```

`-ExecutionPolicy Bypass` 只作用于新启动的 PowerShell 进程。如果企业域策略
通过 `MachinePolicy` 或 `UserPolicy` 强制要求签名或禁止脚本，该入口不会绕过
组织策略；请使用组织认可的签名脚本或联系管理员调整策略。

# Git AI Windows x64 离线安装

解压整个 ZIP，在普通用户 PowerShell 中进入解压后的目录。

首次安装或直接使用包内脚本升级：

```powershell
& .\install.ps1
git-ai --version
```

如果已经把可复用的 `install.ps1` 保存到固定位置：

```powershell
$env:GIT_AI_LOCAL_BINARY = (Resolve-Path -LiteralPath .\windows\git-ai-windows-x64.exe).Path
& 'C:\git-ai-installer\install.ps1'
Remove-Item Env:GIT_AI_LOCAL_BINARY
git-ai --version
```

升级时解压新 ZIP，并让 `GIT_AI_LOCAL_BINARY` 指向新目录中的 exe。正常 CLI 升级无需更换固定的 `install.ps1`。

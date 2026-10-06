# Cyber Master 一键安装脚本（Windows / PowerShell）
#
# 用法（PowerShell 5.1+ / PowerShell 7+）：
#   irm https://raw.githubusercontent.com/chuzouX/cyber-master/main/install.ps1 | iex
#
# 或先下载再执行（适用于离线 / 代理受限环境；必须显式按 UTF-8 读取）：
#   powershell -NoProfile -ExecutionPolicy Bypass -Command "iex (Get-Content -Raw -Encoding UTF8 .\install.ps1)"
#
# 注意：本文件必须保持 **无 BOM 的 UTF-8**，不要用编辑器加回 UTF-8 BOM。
#   - 带 BOM 时 `irm ... | iex` 会把 BOM 并进首个标记，PowerShell 5.1 直接报
#     「无法将“#”项识别为 cmdlet、函数、脚本文件或可运行程序的名称」；
#   - 代价：PowerShell 5.1 直接 `-File` 会按系统 ANSI 代码页解码本文件（中文会乱码
#     甚至解析失败），故本地执行请用上面 `Get-Content -Raw -Encoding UTF8` 的形式；
#     PowerShell 7 的 `-File` 默认按 UTF-8 解码，不受影响。
#
# 注意：脚本内**禁止使用 `exit`**（含 `exit 0` / `exit 1`）。`irm ... | iex` 是在用户
#   自己的 PowerShell 会话里执行本脚本，`exit` 会直接结束宿主进程 —— 表现为「脚本跑完
#   终端窗口自己关了」（例如命中「已是最新版本」或「已取消更新」分支时）。
#   正常/取消退出用 `return`（退出码 0），致命错误用 `throw`（退出码 1，与旧行为一致）。
#
# 高级用法：
#   $env:CYBER_VERSION='v0.1.0'; irm https://raw.githubusercontent.com/.../install.ps1 | iex
#   irm https://raw.githubusercontent.com/.../install.ps1 | iex  # 默认装到 %USERPROFILE%\.local\bin
#
# 若本机已安装 cyber，脚本会先检测已安装版本并与云端最新版本对比：
#   - 已是最新：直接提示并退出（除非 $env:CYBER_FORCE=1）；
#   - 不是最新：询问是否更新到最新版本（回车 / y 确认，n 取消）；
#   - $env:CYBER_FORCE=1：跳过版本检查与询问，直接下载并覆盖安装。
#
# 环境变量覆盖：
#   $env:CYBER_VERSION       指定版本 tag，如 'v0.1.0'
#   $env:CYBER_INSTALL_DIR   安装目录，默认 $env:USERPROFILE\.local\bin
#   $env:CYBER_REPO          GitHub owner/name，默认 chuzouX/cyber-master
#   $env:CYBER_DOWNLOAD_MIRROR 下载镜像前缀（默认自动尝试 ghproxy.net/gh-proxy.com/ghfast.top）
#   $env:CYBER_FORCE         设为 1 时跳过版本检查与询问，直接覆盖安装

#Requires -Version 5.1

# 注意：`irm <url> | iex` 以表达式模式解析脚本，不能使用 param() 块；
# 安装选项统一通过环境变量传入（见下方合并逻辑）。
$Version    = ''
$InstallDir = ''
$Repo       = 'chuzouX/cyber-master'

$ErrorActionPreference = 'Stop'
$ProgressPreference    = 'SilentlyContinue'   # 关闭 Invoke-WebRequest 的进度条，否则慢且管道场景下报错

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'install.ps1 仅支持 Windows；Linux / macOS / WSL 请使用 install.sh。'
}

# ─── 合并环境变量 ──────────────────────────────────────────────────────────
if (-not $Version)    { $Version    = $env:CYBER_VERSION }
if (-not $InstallDir) { $InstallDir = $env:CYBER_INSTALL_DIR }
if (-not $InstallDir) { $InstallDir = Join-Path $env:USERPROFILE '.local\bin' }
if ($env:CYBER_REPO)  { $Repo       = $env:CYBER_REPO }
$Force = $false
if ($env:CYBER_FORCE -and $env:CYBER_FORCE -ne '0') { $Force = $true }
$CnbRepo = if ($env:CYBER_CNB_REPO) { $env:CYBER_CNB_REPO } else { 'funxlink/cyber-master' }
# ─── 平台检测（PowerShell 只支持 Windows 二进制；WSL 用户请用 install.sh）──
$architecture = if ($env:PROCESSOR_ARCHITEW6432) {
    $env:PROCESSOR_ARCHITEW6432
} else {
    $env:PROCESSOR_ARCHITECTURE
}
if ($architecture -ne 'AMD64') {
    throw "不支持的 Windows 架构：$architecture。当前仅发布 x86_64 (AMD64) 二进制。"
}
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
if ($InstallDir.Contains(';')) { throw "安装目录不能含 PATH 分隔符 ';'。" }
$Target = 'x86_64-pc-windows-msvc'
$Archive = "cyber-$Target.zip"

# ─── 解析版本（未指定时取 latest）──────────────────────────────────────────
if (-not $Version) {
    Write-Host "→ 查询最新版本…" -ForegroundColor Cyan
    if ($env:CYBER_USE_CNB) {
        try {
            $cnbReleases = Invoke-RestMethod -Uri "https://api.cnb.cool/$CnbRepo/-/releases" -Headers @{ 'Accept' = 'application/json' } -TimeoutSec 5
            if ($cnbReleases -and $cnbReleases.Count -gt 0) {
                $Version = $cnbReleases[0].tag_name
            }
        } catch {}
    }
    if (-not $Version) {
        try {
            $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" `
                                         -Headers @{ 'User-Agent' = 'cyber-installer' } -TimeoutSec 5
            $Version = $release.tag_name
        } catch {
            try {
                $cnbReleases = Invoke-RestMethod -Uri "https://api.cnb.cool/$CnbRepo/-/releases" -Headers @{ 'Accept' = 'application/json' } -TimeoutSec 5
                if ($cnbReleases -and $cnbReleases.Count -gt 0) {
                    $Version = $cnbReleases[0].tag_name
                }
            } catch {}
        }
    }
    if (-not $Version) {
        # 一律用 throw / return，禁止 exit：本脚本常经 `irm ... | iex` 在用户自己的
        # PowerShell 会话里执行，`exit` 会直接结束宿主进程（终端窗口随之关闭）。
        throw "无法获取最新版本。请设置 `$env:CYBER_VERSION 显式指定版本 tag。"
    }
}

# ─── 已安装版本检测、版本对比与更新确认 ────────────────────────────────────
# 版本号比较：返回 -1 / 0 / 1（忽略 v/V 前缀，按数字段比较）。
function Compare-CyberVersion([string]$A, [string]$B) {
    function ConvertTo-CyberVersionParts([string]$Value) {
        @((($Value.TrimStart('v', 'V')) -split '\.') | ForEach-Object { [int]($_ -replace '\D.*$', '') })
    }
    $pa = ConvertTo-CyberVersionParts $A
    $pb = ConvertTo-CyberVersionParts $B
    for ($i = 0; $i -lt [Math]::Max($pa.Count, $pb.Count); $i++) {
        $x = if ($i -lt $pa.Count) { $pa[$i] } else { 0 }
        $y = if ($i -lt $pb.Count) { $pb[$i] } else { 0 }
        if ($x -lt $y) { return -1 }
        if ($x -gt $y) { return 1 }
    }
    return 0
}

# 询问是否继续；非交互环境下不自动更新。
function Confirm-CyberUpdate([string]$Prompt) {
    if ([Console]::IsInputRedirected) {
        Write-Host '非交互环境，已跳过更新（如需强制覆盖安装请运行: cyber update --force）' -ForegroundColor DarkGray
        return $false
    }
    $answer = ''
    try {
        $answer = Read-Host $Prompt
    } catch {
        Write-Host '非交互环境，已跳过更新（如需强制覆盖安装请运行: cyber update --force）' -ForegroundColor DarkGray
        return $false
    }
    return (-not $answer) -or ($answer -match '^(?i)y(es)?$')
}

$installedPath = ''
$installedCmd = Get-Command cyber -ErrorAction SilentlyContinue
if ($installedCmd) {
    $installedPath = $installedCmd.Source
} elseif (Test-Path -LiteralPath (Join-Path $InstallDir 'cyber.exe') -PathType Leaf) {
    $installedPath = Join-Path $InstallDir 'cyber.exe'
}

if ($installedPath) {
    $installedVersion = ''
    try {
        $rawVersion = & $installedPath --version 2>$null | Select-Object -First 1
        if ($rawVersion) { $installedVersion = ($rawVersion.ToString().Trim() -split '\s+')[-1] }
    } catch {}
    Write-Host "→ 检测到已安装的 cyber: $installedPath" -ForegroundColor Cyan
    $installedLabel = if ($installedVersion) { $installedVersion } else { '未知' }
    Write-Host "  已安装版本: $installedLabel"
    Write-Host "  云端最新版本: $Version"
    if ($Force) {
        Write-Host "→ CYBER_FORCE 已启用：跳过版本检查与询问，直接下载并覆盖安装。" -ForegroundColor Cyan
    } elseif (-not $installedVersion) {
        if (-not (Confirm-CyberUpdate "无法确定已安装版本，是否覆盖安装 $Version？[Y/n]")) {
            Write-Host '已取消更新。'; return
        }
    } elseif ((Compare-CyberVersion $installedVersion $Version) -eq 0) {
        Write-Host "✓ 已是最新版本（$installedVersion），无需更新。" -ForegroundColor Green
        Write-Host '  如需强制覆盖安装，请运行: cyber update --force' -ForegroundColor DarkGray
        return
    } elseif ((Compare-CyberVersion $installedVersion $Version) -lt 0) {
        if (-not (Confirm-CyberUpdate "是否更新到最新版本 $Version？[Y/n]")) {
            Write-Host '已取消更新。'; return
        }
    } else {
        if (-not (Confirm-CyberUpdate "已安装版本 $installedVersion 高于目标版本 $Version，是否覆盖安装？[Y/n]")) {
            Write-Host '已取消更新。'; return
        }
    }
} else {
    Write-Host '→ 未检测到已安装的 cyber，将进行全新安装。' -ForegroundColor DarkGray
}

# ─── 下载源候选列表与多源测速 ──────────────────────────────────────────────
$GithubBase = "https://github.com/$Repo/releases/download/$Version"
$CnbBase    = "https://cnb.cool/$CnbRepo/-/releases/download/$Version"
$CandidateList = @()
if ($env:CYBER_DOWNLOAD_MIRROR) {
    $prefix = $env:CYBER_DOWNLOAD_MIRROR.TrimEnd('/')
    $CandidateList += @{ Name = "自定义镜像 ($prefix)"; Base = "$prefix/$GithubBase" }
}
$CandidateList += @(
    @{ Name = "CNB 镜像源 (国内极速)"; Base = $CnbBase },
    @{ Name = "GitHub 官方源";       Base = $GithubBase },
    @{ Name = "gh-proxy.com 镜像";   Base = "https://gh-proxy.com/$GithubBase" },
    @{ Name = "ghfast.top 镜像";     Base = "https://ghfast.top/$GithubBase" },
    @{ Name = "ghproxy.net 镜像";    Base = "https://ghproxy.net/$GithubBase" }
)
Write-Host "→ 安装 cyber $Version ($Target) 到 $InstallDir" -ForegroundColor Cyan

# ─── 下载、强制 SHA256 校验及安装；所有失败路径均清理临时目录 ─────────────
$tmpDir = Join-Path ([IO.Path]::GetTempPath()) "cyber-install-$([Guid]::NewGuid().ToString('N'))"
$zipFile = Join-Path $tmpDir $Archive
$shaFile = "$zipFile.sha256"
$extractPath = Join-Path $tmpDir 'extract'
$destBinary = Join-Path $InstallDir 'cyber.exe'
try {
    New-Item -ItemType Directory -Path $tmpDir | Out-Null

    Write-Host "→ 测速各下载源并选择最优节点…" -ForegroundColor Cyan
    $TestedSources = @()
    $speedtestSha = Join-Path $tmpDir "speedtest.sha256"

    foreach ($item in $CandidateList) {
        $base = $item.Base
        $name = $item.Name
        $shaUrl = "$base/$Archive.sha256"
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        try {
            Invoke-WebRequest -Uri $shaUrl -OutFile $speedtestSha -UseBasicParsing -TimeoutSec 4
            $sw.Stop()
            $ms = [int]$sw.ElapsedMilliseconds
            $content = (Get-Content -LiteralPath $speedtestSha -TotalCount 1).Trim()
            if ($content -match '^[0-9a-fA-F]{64}') {
                Write-Host ("   [✓] {0,-18} : {1}ms" -f $name, $ms) -ForegroundColor Green
                $TestedSources += [PSCustomObject]@{
                    Name = $name
                    Base = $base
                    Latency = $ms
                }
            } else {
                Write-Host ("   [✗] {0,-18} : 返回内容无效" -f $name) -ForegroundColor DarkGray
            }
        } catch {
            $sw.Stop()
            Write-Host ("   [✗] {0,-18} : 超时/不可达" -f $name) -ForegroundColor DarkGray
        }
    }

    if ($TestedSources.Count -gt 0) {
        $SortedSources = $TestedSources | Sort-Object -Property Latency
        $Fastest = $SortedSources[0]
        Write-Host ("→ 选用最优源: {0} ({1}ms)" -f $Fastest.Name, $Fastest.Latency) -ForegroundColor Cyan
        $Sources = @($SortedSources | ForEach-Object { $_.Base })
    } else {
        Write-Host "   所有源测速未响应，回退至默认源重试…" -ForegroundColor Yellow
        $Sources = @($CandidateList | ForEach-Object { $_.Base })
    }

    $downloaded = $false
    foreach ($base in $Sources) {
        $zipUrl = "$base/$Archive"
        $shaUrl = "$zipUrl.sha256"
        try {
            Write-Host "→ 下载 $zipUrl"
            Invoke-WebRequest -Uri $zipUrl -OutFile $zipFile -UseBasicParsing -TimeoutSec 120
            Invoke-WebRequest -Uri $shaUrl -OutFile $shaFile -UseBasicParsing -TimeoutSec 60
            $downloaded = $true
            break
        } catch {
            Write-Host "   该源下载失败（$($_.Exception.Message.Split([char]10)[0])），尝试备用源…" -ForegroundColor DarkGray
        }
    }
    if (-not $downloaded) {
        throw "所有下载源均失败。请检查网络/代理（需放行 release-assets.githubusercontent.com），或用 `$env:CYBER_DOWNLOAD_MIRROR 指定可用镜像前缀。"
    }
    Write-Host "→ 校验 SHA256…"
    $checksum = Get-Content -LiteralPath $shaFile -TotalCount 1
    if ($checksum -notmatch '^([0-9a-fA-F]{64})(\s|$)') {
        throw 'SHA256 校验文件格式无效，安装已中止。'
    }
    $expected = $Matches[1].ToLowerInvariant()
    $actual = (Get-FileHash -LiteralPath $zipFile -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($expected -ne $actual) {
        throw "SHA256 校验失败：expected=$expected actual=$actual"
    }
    Expand-Archive -LiteralPath $zipFile -DestinationPath $extractPath -Force
    $srcBinary = Join-Path $extractPath 'cyber.exe'
    if (-not (Test-Path -LiteralPath $srcBinary -PathType Leaf)) {
        throw '压缩包内未找到 cyber.exe'
    }
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    # 不删除正在使用的旧二进制；覆盖失败时提示关闭进程后重试。
    try {
        Copy-Item -LiteralPath $srcBinary -Destination $destBinary -Force
    } catch {
        throw "无法写入 $destBinary；请关闭正在运行的 cyber 并检查目录权限后重试。$_"
    }
} finally {
    Remove-Item -LiteralPath $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
}

# ─── 添加到用户 PATH ──────────────────────────────────────────────────────
function Test-CyberPath([string]$PathValue) {
    foreach ($item in ($PathValue -split ';')) {
        if (-not $item.Trim()) { continue }
        $expanded = [Environment]::ExpandEnvironmentVariables($item.Trim().Trim('"'))
        try {
            if ([IO.Path]::GetFullPath($expanded).TrimEnd('\') -eq $InstallDir.TrimEnd('\')) {
                return $true
            }
        } catch {
            # 保留用户原有 PATH 中无法规范化的条目。
        }
    }
    return $false
}
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not (Test-CyberPath $userPath)) {
    $newPath = if ($userPath) { "$userPath;$InstallDir" } else { $InstallDir }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    Write-Host "✓ 已将 $InstallDir 添加到用户 PATH" -ForegroundColor Green
} else {
    Write-Host "✓ $InstallDir 已在用户 PATH 中" -ForegroundColor DarkGray
}
# 即便持久用户 PATH 已有该目录，当前会话也可能尚未继承它。
if (-not (Test-CyberPath $env:Path)) {
    $env:Path = if ($env:Path) { "$env:Path;$InstallDir" } else { $InstallDir }
}
Write-Host "  请重新打开其他终端；当前 PowerShell 会话的 PATH 已同步。" -ForegroundColor DarkGray

# ─── 完成提示 ────────────────────────────────────────────────────────────
Write-Host ""
Write-Host "✓ 已安装: $destBinary" -ForegroundColor Green
Write-Host "  运行: cyber" -ForegroundColor Green
Write-Host ""
Write-Host "首次运行 cyber 会自动在 $env:USERPROFILE\.cyber\ 创建配置目录。" -ForegroundColor DarkGray
Write-Host "文档: https://github.com/$Repo#readme" -ForegroundColor DarkGray

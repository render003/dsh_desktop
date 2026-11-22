#Requires -Version 5.1
<#
.SYNOPSIS
  本机 Windows 一键增量编译并同步 DSH Pack 到安装目录（免全量 NSIS 打包/重装）。

.DESCRIPTION
  遵循 AGENTS.md 本机 Windows Rust 构建缓存约定：
  - 不设置 CARGO_TARGET_DIR 或 --target-dir，沿用用户级 Cargo 配置（E:/build-cache/cargo-target）；
  - 通过 cargo metadata 动态解析 target_directory 定位 dsh-tauri-app.exe；
  - 先完成编译，再在复制前的 1~2 秒关闭正在运行的安装版进程，减少中断时间；
  - 增量镜像 sidecar / ui / dsh-desktop 根脚本 / scripts / assets，并对安装目录执行
    patch-deps.js 使新补丁直接落盘，不触碰大体积的 vendor/node 与 node_modules。

.PARAMETER InstallDir
  目标安装目录，默认 $env:LOCALAPPDATA\DSH Desktop。

.PARAMETER SkipBuild
  跳过 Rust release 编译（仅改了 dsh-desktop/scripts、assets/plugins 或 sidecar 时使用）。

.PARAMETER IncludeNodeModules
  同时增量同步 vendor/dsh-kernel 与生产 node_modules（仅升级上游内核版本时需要）。

.PARAMETER NoStop
  检测到安装版正在运行时直接报错退出，不自动关闭。

.PARAMETER Launch
  同步完成后自动重新启动安装版 DSH Pack。

.PARAMETER Target
  可选 Cargo target 三元组（如 x86_64-pc-windows-msvc）；留空使用默认宿主三元组。

.EXAMPLE
  # 全量增量同步（编 release exe + 同步脚本/插件 + 自动关旧启新）
  pwsh -File dsh-tauri/scripts/sync-installed.ps1 -Launch

.EXAMPLE
  # 只改了插件或补丁脚本（跳过 Rust 编译，约 2 秒完成）
  pwsh -File dsh-tauri/scripts/sync-installed.ps1 -SkipBuild -Launch
#>
[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'DSH Desktop'),
    [switch]$SkipBuild,
    [switch]$IncludeNodeModules,
    [switch]$NoStop,
    [switch]$Launch,
    [string]$Target = ''
)

$ErrorActionPreference = 'Stop'

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$ManifestPath = Join-Path $RepoRoot 'dsh-tauri\src-tauri\Cargo.toml'
$SrcDesktop = Join-Path $RepoRoot 'dsh-desktop'
$SrcSidecar = Join-Path $RepoRoot 'dsh-tauri\sidecar'
$SrcUi = Join-Path $RepoRoot 'dsh-tauri\ui'
$DstDesktop = Join-Path $InstallDir 'dsh-desktop'

function Invoke-Mirror {
    param(
        [Parameter(Mandatory)][string]$Source,
        [Parameter(Mandatory)][string]$Destination,
        [string[]]$ExtraArgs = @()
    )
    if (-not (Test-Path $Source)) {
        throw "源目录不存在: $Source"
    }
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    $rcArgs = @($Source, $Destination, '/MIR', '/R:2', '/W:1', '/NFL', '/NDL', '/NJH', '/NJS', '/NP') + $ExtraArgs
    & robocopy @rcArgs | Out-Null
    $code = $LASTEXITCODE
    if ($code -ge 8) {
        throw "robocopy 失败 (exit $code): $Source -> $Destination"
    }
}

function Resolve-CargoTargetDir {
    if ($env:CARGO_TARGET_DIR) {
        Write-Warning "检测到环境变量 CARGO_TARGET_DIR=$($env:CARGO_TARGET_DIR)，将影响用户级共享缓存约定。"
    }
    $metaJson = & cargo metadata --manifest-path $ManifestPath --format-version 1 --no-deps
    if ($LASTEXITCODE -ne 0) {
        throw 'cargo metadata 执行失败'
    }
    $meta = $metaJson | ConvertFrom-Json
    return [string]$meta.target_directory
}

function Find-BuiltExe {
    param(
        [Parameter(Mandatory)][string]$TargetDir,
        [string]$Triple
    )
    $candidates = @()
    if ($Triple) {
        $candidates += (Join-Path $TargetDir "$Triple\release\dsh-tauri-app.exe")
    } else {
        $candidates += (Join-Path $TargetDir 'release\dsh-tauri-app.exe')
        $candidates += (Join-Path $TargetDir 'x86_64-pc-windows-msvc\release\dsh-tauri-app.exe')
    }
    foreach ($c in $candidates) {
        if (Test-Path $c) { return $c }
    }
    throw "未找到编译产物 dsh-tauri-app.exe（已查：$($candidates -join ', ')）"
}

function Stop-InstalledApp {
    param([Parameter(Mandatory)][string]$TargetExePath)
    $normTarget = [System.IO.Path]::GetFullPath($TargetExePath)
    $procs = @(Get-Process -Name 'dsh-tauri-app' -ErrorAction SilentlyContinue | Where-Object {
        try {
            $_.Path -and ([System.IO.Path]::GetFullPath($_.Path) -ieq $normTarget)
        } catch {
            $true
        }
    })
    if ($procs.Count -eq 0) { return }
    if ($NoStop) {
        throw "检测到 DSH Pack 正在运行（PID: $(($procs.Id) -join ', ')），请先从托盘完全退出后重试，或去掉 -NoStop 让脚本自动关闭。"
    }
    Write-Host "[sync] 检测到安装版正在运行（PID: $(($procs.Id) -join ', ')），正在安全停止以释放文件锁..."
    foreach ($p in $procs) {
        & taskkill /PID $p.Id /T /F 2>$null | Out-Null
    }
    # 等待 exe 句柄彻底释放（最多 5 秒）
    $deadline = (Get-Date).AddSeconds(5)
    while ((Get-Date) -lt $deadline) {
        $alive = @(Get-Process -Name 'dsh-tauri-app' -ErrorAction SilentlyContinue | Where-Object {
            try { $_.Path -and ([System.IO.Path]::GetFullPath($_.Path) -ieq $normTarget) } catch { $false }
        })
        if ($alive.Count -eq 0) { break }
        Start-Sleep -Milliseconds 200
    }
}

# ---- 前置检查 ----
if (-not (Test-Path $DstDesktop)) {
    throw "未找到已安装的 DSH Pack 目录: $DstDesktop（请先完成首次安装或用 -InstallDir 指定路径）"
}

$BuiltExe = $null
if (-not $SkipBuild) {
    $TargetDir = Resolve-CargoTargetDir
    Write-Host "[sync] Cargo target_directory = $TargetDir"

    # tauri-build 编译期校验 tauri.conf.json 中的 ../../../pp 存在
    $PpDir = Join-Path $RepoRoot 'dsh-tauri\pp'
    if (-not (Test-Path $PpDir)) {
        New-Item -ItemType Directory -Force -Path $PpDir | Out-Null
    }

    $buildArgs = @('build', '--manifest-path', $ManifestPath, '-p', 'dsh-tauri-app', '--release')
    if ($Target) {
        $buildArgs += @('--target', $Target)
    }
    Write-Host "[sync] 编译 release 可执行文件: cargo $($buildArgs -join ' ')"
    & cargo @buildArgs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build 失败 (exit $LASTEXITCODE)"
    }
    $BuiltExe = Find-BuiltExe -TargetDir $TargetDir -Triple $Target
    Write-Host "[sync] 编译产物就绪: $BuiltExe"
}

# ---- 停止旧进程并同步 ----
$InstalledExe = Join-Path $InstallDir 'dsh-tauri-app.exe'
Stop-InstalledApp -TargetExePath $InstalledExe

if ($BuiltExe) {
    Write-Host "[sync] 更新可执行文件 -> $InstalledExe"
    Copy-Item -Force -Path $BuiltExe -Destination $InstalledExe
}

Write-Host '[sync] 增量同步 sidecar/ 与 ui/ ...'
Invoke-Mirror -Source $SrcSidecar -Destination (Join-Path $InstallDir 'sidecar') -ExtraArgs @('/XF', '*.test.js')
Invoke-Mirror -Source $SrcUi -Destination (Join-Path $InstallDir 'ui')

Write-Host '[sync] 增量同步 dsh-desktop 根脚本 / scripts/ / assets/ ...'
Get-ChildItem -Path $SrcDesktop -File -Filter '*.js' | ForEach-Object {
    Copy-Item -Force -Path $_.FullName -Destination (Join-Path $DstDesktop $_.Name)
}
Copy-Item -Force -Path (Join-Path $SrcDesktop 'package.json') -Destination (Join-Path $DstDesktop 'package.json')

Invoke-Mirror -Source (Join-Path $SrcDesktop 'scripts') -Destination (Join-Path $DstDesktop 'scripts') -ExtraArgs @('/XD', 'test')
Invoke-Mirror -Source (Join-Path $SrcDesktop 'assets') -Destination (Join-Path $DstDesktop 'assets') -ExtraArgs @('/XD', '.pnpm')

# 清理安装目录可能残留的已裁撤资源目录
foreach ($retired in @('assets\sponsor', 'assets\plugins\ds-bot', 'assets\plugins\ds-harness-remote')) {
    $rp = Join-Path $DstDesktop $retired
    if (Test-Path $rp) { Remove-Item -Recurse -Force $rp }
}

if ($IncludeNodeModules) {
    Write-Host '[sync] 同步 vendor/dsh-kernel/ 与生产 node_modules/ ...'
    Invoke-Mirror -Source (Join-Path $SrcDesktop 'vendor\dsh-kernel') -Destination (Join-Path $DstDesktop 'vendor\dsh-kernel')
    Invoke-Mirror -Source (Join-Path $SrcDesktop 'node_modules') -Destination (Join-Path $DstDesktop 'node_modules') -ExtraArgs @(
        '/XD', 'electron', 'electron-builder', 'electron-winstaller', '@electron',
        'sharp-darwin-arm64', 'sharp-libvips-darwin-arm64', 'sharp-wasm32', 'koffi-darwin-arm64',
        'codex-win32-x64', 'claude-agent-sdk-win32-x64'
    )
}

# 对安装目录重放内核补丁（幂等，使新补丁立即落盘到已安装的 node_modules）
$NodeExe = Join-Path $DstDesktop 'vendor\node\node.exe'
if (-not (Test-Path $NodeExe)) { $NodeExe = 'node' }
$PatchScript = Join-Path $DstDesktop 'scripts\patch-deps.js'
Write-Host "[sync] 对安装目录应用内核补丁: $PatchScript"
& $NodeExe $PatchScript
if ($LASTEXITCODE -ne 0) {
    throw "安装目录 patch-deps.js 执行失败 (exit $LASTEXITCODE)"
}

Write-Host "[sync] 增量同步完成 -> $InstallDir"

if ($Launch) {
    Write-Host "[sync] 重新启动 DSH Pack: $InstalledExe"
    Start-Process -FilePath $InstalledExe -WorkingDirectory $InstallDir
}

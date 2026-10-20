# 打包 Vivarium 可执行文件
#
#   .\scripts\package.ps1              # 打包图形前端到 dist\vivarium.exe
#   .\scripts\package.ps1 -IncludeSim  # 顺带打包 headless 模拟器
#   .\scripts\package.ps1 -SkipBuild   # 不重新编译，只复制现有产物
#
# 约定：**每完成一个版本就打一次包**，dist\ 下的 exe 是"可以直接双击"的那份。
# dist/ 不入库（见 .gitignore），仓库里只有源码与这个脚本。

[CmdletBinding()]
param(
    [switch]$IncludeSim,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent $PSScriptRoot
$dist = Join-Path (Split-Path -Parent $repo) 'dist'
$release = Join-Path $repo 'target\release'

function Write-Step($text) { Write-Host "==> $text" -ForegroundColor Cyan }

# --- 1. 编译 ---------------------------------------------------------------
if (-not $SkipBuild) {
    Write-Step '编译 release（-j 4，这台机器满载会过热）'
    Push-Location $repo
    try {
        # 显式 -j 4：并行度拉满会让 CPU 长时间 100%，这台机器扛不住。
        cargo build --release -j 4
        if ($LASTEXITCODE -ne 0) { throw "cargo build 失败（exit $LASTEXITCODE）" }
    } finally {
        Pop-Location
    }
}

# --- 2. 目标进程还在跑就先停掉，否则复制会失败 -----------------------------
$running = Get-Process -Name 'vivarium' -ErrorAction SilentlyContinue
if ($running) {
    Write-Step "dist\vivarium.exe 正在运行，先结束它"
    $running | Stop-Process -Force
    Start-Sleep -Milliseconds 300
}

# --- 3. 复制 ---------------------------------------------------------------
if (-not (Test-Path $dist)) {
    New-Item -ItemType Directory -Path $dist | Out-Null
}

$game = Join-Path $release 'vivarium-game.exe'
if (-not (Test-Path $game)) { throw "找不到 $game，先跑一次不带 -SkipBuild 的打包" }
Copy-Item $game -Destination (Join-Path $dist 'vivarium.exe') -Force
Write-Step "dist\vivarium.exe  ($([math]::Round((Get-Item (Join-Path $dist 'vivarium.exe')).Length / 1MB, 1)) MB)"

if ($IncludeSim) {
    $sim = Join-Path $release 'vivarium-sim.exe'
    if (Test-Path $sim) {
        Copy-Item $sim -Destination (Join-Path $dist 'vivarium-sim.exe') -Force
        Write-Step "dist\vivarium-sim.exe  ($([math]::Round((Get-Item (Join-Path $dist 'vivarium-sim.exe')).Length / 1MB, 1)) MB)"
    }
}

# --- 4. 记一笔"这是哪一版" -------------------------------------------------
#
# 中文提交信息会变成乱码，除非在调 git 之前把控制台的输出编码改成 UTF-8。
# PowerShell 解码外部程序的 stdout 时用的是 [Console]::OutputEncoding，
# 而它在中文 Windows 上默认是 GBK(936)，于是 git 吐出的 UTF-8 字节被按 GBK 解——
# VERSION.txt 里就成了"璁╃粏鑳炵湅寰楄"。这不是 git 的问题，是宿主的问题。
$prevOut = [Console]::OutputEncoding
try {
    [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
    $OutputEncoding = [System.Text.Encoding]::UTF8

    Push-Location $repo
    try {
        $commit = (git rev-parse --short HEAD 2>$null)
        $dirty = if (git status --porcelain 2>$null) { '（工作区有未提交改动）' } else { '' }
        $subject = (git log -1 --pretty=%s 2>$null)
    } finally {
        Pop-Location
    }
} finally {
    [Console]::OutputEncoding = $prevOut
}

$stamp = Get-Date -Format 'yyyy-MM-dd HH:mm'
$info = @(
    "Vivarium $commit$dirty"
    "构建时间 $stamp"
    "提交信息 $subject"
) -join "`r`n"
Set-Content -Path (Join-Path $dist 'VERSION.txt') -Value $info -Encoding UTF8

Write-Host ''
Write-Host $info -ForegroundColor DarkGray
Write-Host ''
Write-Step '完成。双击 dist\vivarium.exe 即可运行（按 L 放鱼）'

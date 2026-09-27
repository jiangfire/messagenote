#requires -Version 7
<#
.SYNOPSIS
  跑网页端的真实浏览器端到端测试。

.DESCRIPTION
  把「起 Rust 服务端 → 起同源静态服务器 → 起 headless 浏览器 → 跑用例 → 收尾」
  打包成一件事。本地和 CI 共用同一份 —— 手工敲五条命令总会漏掉清理，
  留下一堆占着端口的进程，下次跑就莫名其妙地失败。

  前置：pnpm build 和 cargo build --release -p messagenote-server 都已经跑过。

.EXAMPLE
  pwsh scripts/web-e2e/run.ps1
#>
param(
  [string]$Dist = "dist",
  [string]$ServerExe = "target/release/messagenote-server.exe",
  [string]$Token = "browser-test-token-0123456789abcdefghijkl",
  [int]$ApiPort = 8803,
  [int]$WebPort = 8804,
  [int]$CdpPort = 9222,
  [string]$Shots = "scripts/web-e2e/shots"
)

$ErrorActionPreference = "Stop"
$root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
Set-Location $root

if (-not (Test-Path $ServerExe)) { throw "找不到服务端二进制：$ServerExe（先 cargo build --release -p messagenote-server）" }
if (-not (Test-Path (Join-Path $Dist "web.html"))) { throw "找不到 $Dist/web.html（先 pnpm build）" }

# 找一个 Chromium 系的浏览器。Windows runner 自带 Edge，本地多半也有。
$edge = @(
  "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe",
  "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe",
  "$env:LOCALAPPDATA\Google\Chrome\Application\chrome.exe",
  "$env:ProgramFiles\Google\Chrome\Application\chrome.exe"
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $edge) { throw "找不到 Chrome 或 Edge —— 这个测试必须在真实浏览器里跑" }

$work = Join-Path $root ".scratch\web-e2e-run"
Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $work | Out-Null

$serverDb = Join-Path $work "server.sqlite"
$profile = Join-Path $work "browser-profile"
$serverLog = Join-Path $work "server.log"

$apiUrl = "http://127.0.0.1:$ApiPort"
$appUrl = "http://127.0.0.1:$WebPort/"

Write-Output "浏览器: $edge"
Write-Output "服务端: $apiUrl   网页端: $appUrl"

$procs = @()
try {
  # ---- 服务端 ----
  $env:MESSAGENOTE_TOKEN = $Token
  $env:MESSAGENOTE_DB = $serverDb
  $env:MESSAGENOTE_BIND = "127.0.0.1:$ApiPort"
  $procs += Start-Process -FilePath (Resolve-Path $ServerExe) -PassThru `
    -RedirectStandardOutput $serverLog -RedirectStandardError "$serverLog.err"

  # ---- 同源静态服务器（托管 dist/ 并反代 /api）----
  $procs += Start-Process -FilePath "node" -PassThru `
    -ArgumentList @("scripts/web-e2e/serve.mjs", (Resolve-Path $Dist).Path, $apiUrl, "$WebPort")

  # ---- headless 浏览器 ----
  $procs += Start-Process -FilePath $edge -PassThru -ArgumentList @(
    "--headless=new", "--disable-gpu", "--no-first-run", "--no-default-browser-check",
    "--remote-debugging-port=$CdpPort", "--user-data-dir=$profile", "about:blank"
  )

  # 等两边都就绪。直接跑用例的话，第一次 fetch 会打在一个还没 listen 的端口上。
  $ready = $false
  foreach ($_ in 1..40) {
    Start-Sleep -Milliseconds 500
    try {
      $null = Invoke-WebRequest -Uri "$appUrl/api/health" -UseBasicParsing -TimeoutSec 2
      $null = Invoke-RestMethod -Uri "http://127.0.0.1:$CdpPort/json/version" -TimeoutSec 2
      $ready = $true
      break
    } catch { }
  }
  if (-not $ready) {
    Write-Output "=== 服务端日志 ==="
    Get-Content $serverLog -ErrorAction SilentlyContinue | Select-Object -Last 20
    throw "服务端或浏览器在 20 秒内没起来"
  }

  node scripts/web-e2e/browser-e2e.mjs $appUrl $Token $Shots
  $code = $LASTEXITCODE

  if ($code -ne 0) {
    Write-Output "=== 服务端日志 ==="
    Get-Content $serverLog -ErrorAction SilentlyContinue | Select-Object -Last 20
  }
  exit $code
} finally {
  foreach ($p in $procs) {
    if ($p -and -not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
  }
  # 浏览器会派生子进程，父进程杀掉之后子进程还在
  Get-Process msedge, chrome -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq $edge } | Stop-Process -Force -ErrorAction SilentlyContinue
  Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}

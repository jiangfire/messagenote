# 一条命令跑完「服务端附件放对象存储」的端到端验证。
#
#   pwsh -NoProfile -File scripts/s3-e2e/run.ps1
#
# 它做三件事：
#   1. 起一个 **moto** 的 S3 服务（本机 HTTP + 真 SigV4 + 路径式寻址）
#   2. 用 MESSAGENOTE_S3_* 起**真的** messagenote-server 二进制
#   3. 跑 e2e.py，断言桶里的对象、往返字节、missing 的答案、以及老数据的穿底读
#
# 前提：
#   - `pip install "moto[s3,server]"`（boto3 会跟着装上）
#   - `cargo build -p messagenote-server`
#   - 这两个端口没被占：5099（moto）、8799（服务端）

param(
  [string]$Python = "python",
  [int]$S3Port = 5099,
  [int]$ApiPort = 8799,
  [string]$Work = ""
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
if ([string]::IsNullOrWhiteSpace($Work)) { $Work = Join-Path $repo ".scratch\s3-e2e" }
New-Item -ItemType Directory -Force -Path $Work | Out-Null

$server = Join-Path $repo "target\debug\messagenote-server.exe"
if (-not (Test-Path $server)) {
  Write-Host "先构建：cargo build -p messagenote-server" -ForegroundColor Red
  exit 2
}

# moto 在不在？不在就把该装的命令说出来，别让人去看一堆 import 报错。
& $Python -c "import moto, boto3" 2>$null
if ($LASTEXITCODE -ne 0) {
  Write-Host "缺依赖：$Python -m pip install ""moto[s3,server]""" -ForegroundColor Red
  exit 2
}

$db = Join-Path $Work "server.sqlite"
Remove-Item "$db*" -Force -ErrorAction SilentlyContinue

$moto = $null
$srv = $null
try {
  Write-Host "起 moto（S3 端点 127.0.0.1:$S3Port）…"
  $moto = Start-Process $Python -ArgumentList "-m", "moto.server", "-p", "$S3Port" -PassThru `
    -RedirectStandardOutput "$Work\moto.out" -RedirectStandardError "$Work\moto.err"
  Start-Sleep -Seconds 5

  Write-Host "起 messagenote-server（附件放 S3）…"
  $env:MESSAGENOTE_DB = $db
  $env:MESSAGENOTE_TOKEN = "e2e-s3-token-0123456789abcdefghijkl"
  $env:MESSAGENOTE_BIND = "127.0.0.1:$ApiPort"
  $env:MESSAGENOTE_S3_BUCKET = "mn-attachments"
  $env:MESSAGENOTE_S3_ENDPOINT = "http://127.0.0.1:$S3Port"
  $env:MESSAGENOTE_S3_REGION = "us-east-1"
  $env:MESSAGENOTE_S3_ACCESS_KEY_ID = "test"
  $env:MESSAGENOTE_S3_SECRET_ACCESS_KEY = "test"
  $env:RUST_LOG = "info"
  $srv = Start-Process $server -PassThru `
    -RedirectStandardOutput "$Work\server.out" -RedirectStandardError "$Work\server.err"
  Start-Sleep -Seconds 2

  & $Python (Join-Path $PSScriptRoot "e2e.py") `
    --db $db `
    --base "http://127.0.0.1:$ApiPort" `
    --endpoint "http://127.0.0.1:$S3Port"
  $code = $LASTEXITCODE

  # 启动那行日志里带着 `blobs=S3（对象存储）` —— 出问题时先看它
  Write-Host "`n服务端启动日志：" -ForegroundColor DarkGray
  Get-Content "$Work\server.out" -ErrorAction SilentlyContinue | Select-Object -First 3
} finally {
  # 只停自己起的进程（按句柄，不按名字匹配 —— 名字匹配会误伤你自己在跑的实例）
  foreach ($p in @($srv, $moto)) {
    if ($p -and -not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
  }
}

if ($code -ne 0) { exit $code }
Write-Host "`n全部通过" -ForegroundColor Green

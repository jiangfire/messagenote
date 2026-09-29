# 在真实桌面端验证「粘贴图片 → 附件落库 → 字节与 sha 对得上」。
#
# 目的：在**真实桌面端**上验证「粘贴图片 → 附件落库 → 字节与 sha 对得上」这条链路。
# 这是 ROADMAP「验证债」里那三条之一：有测试覆盖，但从没在真机上点过。
#
# 为什么必须是真的：这条路径上有两处只有真机才会暴露的东西 ——
#  1. WebView 在粘贴时到底给不给图片数据（浏览器 E2E 验的是网页端，不是 Tauri）；
#  2. `read_attachment` 走的是 `tauri::ipc::Response`（原始字节通道），而不是
#     `Vec<u8>`（会被序列化成 JSON 数组）。后者在字节层面就可能悄悄出错。
#
# 做法沿用 verify-capture.ps1 的思路：**合成真实的键盘输入**（keybd_event），
# 而不是直接调内部函数 —— 验的是用户真正会走的那条路。
#
# 三条护栏（抄自 verify-capture.ps1，第 2、3 条是本脚本新加的）：
#  1. 先归一化浮层状态：快捷键是「切换」语义，先收起再唤起，否则会把"收起"误判成失败。
#  2. 只有在确认浮层**已经可见且处于前台**之后才发后续按键，否则立刻中止
#     —— 不然粘贴和回车会打到当时前台的那个窗口里去。
#  3. **跑之前保存剪贴板，跑完还原**，不弄丢用户复制的东西。

param(
  [int]$ProbeSize = 12,
  [int]$SettleMs = 1200,
  [switch]$SkipClipboardRestore
)

$ErrorActionPreference = 'Stop'

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class KeySim {
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, IntPtr extra);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);

  public const byte VK_CONTROL = 0x11;
  public const byte VK_SHIFT   = 0x10;
  public const byte VK_SPACE   = 0x20;
  public const byte VK_V       = 0x56;
  public const byte VK_RETURN  = 0x0D;
  public const uint KEYUP      = 0x0002;
  public const string OVERLAY_TITLE = "快速记录";

  public static void Down(byte vk) { keybd_event(vk, 0, 0, IntPtr.Zero); }
  public static void Up(byte vk)   { keybd_event(vk, 0, KEYUP, IntPtr.Zero); }

  public static string ForegroundTitle() {
    StringBuilder sb = new StringBuilder(512);
    GetWindowText(GetForegroundWindow(), sb, 512);
    return sb.ToString();
  }

  private static uint wantPid;
  private static bool found;
  private static bool Probe(IntPtr h, IntPtr l) {
    uint pid;
    GetWindowThreadProcessId(h, out pid);
    if (pid == wantPid && IsWindowVisible(h)) {
      StringBuilder sb = new StringBuilder(256);
      GetWindowText(h, sb, 256);
      if (sb.ToString() == OVERLAY_TITLE) found = true;
    }
    return true;
  }
  public static bool IsOverlayVisible(uint pid) {
    wantPid = pid; found = false;
    EnumWindows(Probe, IntPtr.Zero);
    return found;
  }
}
"@

function Send-Hotkey {
  [KeySim]::Down([KeySim]::VK_CONTROL)
  [KeySim]::Down([KeySim]::VK_SHIFT)
  [KeySim]::Down([KeySim]::VK_SPACE)
  Start-Sleep -Milliseconds 60
  [KeySim]::Up([KeySim]::VK_SPACE)
  [KeySim]::Up([KeySim]::VK_SHIFT)
  [KeySim]::Up([KeySim]::VK_CONTROL)
}

function Send-CtrlV {
  [KeySim]::Down([KeySim]::VK_CONTROL)
  [KeySim]::Down([KeySim]::VK_V)
  Start-Sleep -Milliseconds 60
  [KeySim]::Up([KeySim]::VK_V)
  [KeySim]::Up([KeySim]::VK_CONTROL)
}

function Send-Enter {
  [KeySim]::Down([KeySim]::VK_RETURN)
  Start-Sleep -Milliseconds 60
  [KeySim]::Up([KeySim]::VK_RETURN)
}

# ---------------------------------------------------------------- 0) 应用在跑吗

$proc = Get-Process messagenote -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $proc) {
  Write-Output "应用没在运行。先启动它，再跑这个脚本 —— 启动和合成按键必须分成两步，"
  Write-Output "否则窗口还没准备好就开始发按键，会打得一塌糊涂。"
  exit 1
}
$appPid = [uint32]$proc.Id
Write-Output "应用 PID: $appPid"

# ---------------------------------------------------------------- 1) 保存剪贴板

$savedText = $null
$savedImage = $null
$savedFiles = $null
try { $savedText = Get-Clipboard -Raw } catch { }
try { if ([System.Windows.Forms.Clipboard]::ContainsImage()) { $savedImage = [System.Windows.Forms.Clipboard]::GetImage() } } catch { }
try { if ([System.Windows.Forms.Clipboard]::ContainsFileDropList()) { $savedFiles = [System.Windows.Forms.Clipboard]::GetFileDropList() } } catch { }

Write-Output ""
Write-Output "=== 1) 剪贴板已保存（跑完还原）==="
Write-Output "  文本: $(if ($savedText) { "$($savedText.Length) 字符" } else { '无' })"
Write-Output "  图片: $(if ($savedImage) { "$($savedImage.Width)x$($savedImage.Height)" } else { '无' })"
Write-Output "  文件: $(if ($savedFiles -and $savedFiles.Count) { "$($savedFiles.Count) 个" } else { '无' })"

function Restore-Clipboard {
  if ($SkipClipboardRestore) { Write-Output "  （按参数要求跳过还原）"; return }
  try {
    # 按"信息量从多到少"还原：图片/文件优先，最后才是文本。
    # 一次 Set 只能给一种格式，所以只还原当初真正有的那一种。
    if ($savedImage) {
      [System.Windows.Forms.Clipboard]::SetImage($savedImage)
      Write-Output "  已还原：图片 $($savedImage.Width)x$($savedImage.Height)"
    } elseif ($savedFiles -and $savedFiles.Count) {
      [System.Windows.Forms.Clipboard]::SetFileDropList($savedFiles)
      Write-Output "  已还原：$($savedFiles.Count) 个文件"
    } elseif ($savedText) {
      Set-Clipboard -Value $savedText
      Write-Output "  已还原：文本 $($savedText.Length) 字符"
    } else {
      [System.Windows.Forms.Clipboard]::Clear()
      Write-Output "  已还原：清空（原本就是空的）"
    }
  } catch {
    Write-Output "  !! 还原失败：$($_.Exception.Message) —— 原来的剪贴板内容可能丢了"
  }
}

# ---------------------------------------------------------------- 2) 生成测试图

$png = Join-Path $PSScriptRoot "paste-probe-$ProbeSize.png"
$bmp = New-Object System.Drawing.Bitmap $ProbeSize, $ProbeSize
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.Clear([System.Drawing.Color]::FromArgb(255, 30, 120, 200))
$g.FillRectangle([System.Drawing.Brushes]::Orange, 1, 1, $ProbeSize - 2, $ProbeSize - 2)
$g.Dispose()
$bmp.Save($png, [System.Drawing.Imaging.ImageFormat]::Png)

Write-Output ""
Write-Output "=== 2) 测试图已生成 ==="
Write-Output "  $png  ($ProbeSize x $ProbeSize)"
Write-Output "  它的 sha256 事先算不出来 —— 剪贴板走的是 DIB，浏览器会重新编码成 PNG，"
Write-Output "  所以断言是「库里的字节 sha256 == 正文里写的 sha」+「解出来真的是 $ProbeSize x $ProbeSize」。"

# ---------------------------------------------------------------- 3) 归一化浮层

Write-Output ""
Write-Output "=== 3) 归一化：确保浮层处于收起状态 ==="
if ([KeySim]::IsOverlayVisible($appPid)) {
  Write-Output "  浮层当前可见（切换语义），先按一次收起"
  Send-Hotkey
  Start-Sleep -Milliseconds 800
}
if ([KeySim]::IsOverlayVisible($appPid)) {
  Write-Output "  仍在显示，异常"; Restore-Clipboard; exit 1
}
Write-Output "  已收起"

# ---------------------------------------------------------------- 4) 图片进剪贴板

Write-Output ""
Write-Output "=== 4) 把测试图放进剪贴板 ==="
[System.Windows.Forms.Clipboard]::SetImage($bmp)
Write-Output "  ContainsImage = $([System.Windows.Forms.Clipboard]::ContainsImage())"

# ---------------------------------------------------------------- 5) 唤起浮层

Write-Output ""
Write-Output "=== 5) 合成 Ctrl+Shift+Space ==="
Write-Output "  按下前前台: '$([KeySim]::ForegroundTitle())'"
Send-Hotkey
Start-Sleep -Milliseconds $SettleMs
$fg = [KeySim]::ForegroundTitle()
$vis = [KeySim]::IsOverlayVisible($appPid)
Write-Output "  按下后前台: '$fg'   浮层可见: $vis"

if ($fg -ne [KeySim]::OVERLAY_TITLE -or -not $vis) {
  Write-Output ""
  Write-Output "!! 浮层没有取得前台焦点。已中止，不发送后续按键，以免误输入到其它窗口。"
  Restore-Clipboard
  exit 2
}

# ---------------------------------------------------------------- 6) 粘贴 + 发送

Write-Output ""
Write-Output "=== 6) Ctrl+V 粘贴图片 ==="
Send-CtrlV
Start-Sleep -Milliseconds 1500

Write-Output "=== 7) 回车发送 ==="
Send-Enter
Start-Sleep -Milliseconds 1500
Write-Output "  发送后前台: '$([KeySim]::ForegroundTitle())'"
Write-Output "  发送后浮层可见: $([KeySim]::IsOverlayVisible($appPid))  （期望 False = 已自动收起）"

# ---------------------------------------------------------------- 8) 还原剪贴板

Write-Output ""
Write-Output "=== 8) 还原剪贴板 ==="
Restore-Clipboard

# ---------------------------------------------------------------- 9) 查库断言

Write-Output ""
Write-Output "=== 9) 查库：正文里的 sha 和字节对不对得上 ==="
python (Join-Path $PSScriptRoot 'paste-db.py') --expect-size $ProbeSize
$code = $LASTEXITCODE
Write-Output ""
Write-Output "=== 完成（断言退出码 $code）==="
exit $code

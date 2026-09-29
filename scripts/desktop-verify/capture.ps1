# 一次性验证脚本
#
# 把指定标题的窗口拉到前台并截图。
#
# 关键点：必须先声明进程 DPI 感知。否则在高 DPI 缩放下
# GetWindowRect / CopyFromScreen 拿到的是被虚拟化的坐标，
# 截出来的图会整体错位（表现为窗口内容"跑到画面外"）。
#
# 用 PrintWindow 而不是 CopyFromScreen：前者直接向窗口要一份离屏渲染结果，
# 不受其它窗口遮挡影响，也不依赖 SetForegroundWindow 是否成功。

param(
  [string]$Title = "MessageNote"
)

# 回调是在委托里执行的，用 script 作用域显式传递，避免作用域解析的意外
$script:wantTitle = $Title

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class WinCap {
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr p);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
}
"@

[void][WinCap]::SetProcessDPIAware()

$p = Get-Process messagenote -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $p) { Write-Output "进程不存在"; exit 1 }
$target = [uint32]$p.Id

$script:hwnd = [IntPtr]::Zero
$cb = [WinCap+EnumWindowsProc]{
  param([IntPtr]$h, [IntPtr]$l)
  $owner = [uint32]0
  [void][WinCap]::GetWindowThreadProcessId($h, [ref]$owner)
  if ($owner -eq $target -and [WinCap]::IsWindowVisible($h)) {
    $t = New-Object System.Text.StringBuilder 512
    [void][WinCap]::GetWindowText($h, $t, 512)
    if ($t.ToString() -eq $script:wantTitle) { $script:hwnd = $h }
  }
  return $true
}
[void][WinCap]::EnumWindows($cb, [IntPtr]::Zero)

if ($script:hwnd -eq [IntPtr]::Zero) { Write-Output "没找到可见的窗口：$script:wantTitle"; exit 1 }

[void][WinCap]::ShowWindow($script:hwnd, 9)   # SW_RESTORE
[void][WinCap]::SetForegroundWindow($script:hwnd)
Start-Sleep -Milliseconds 1200

$r = New-Object WinCap+RECT
[void][WinCap]::GetWindowRect($script:hwnd, [ref]$r)
$w = $r.Right - $r.Left
$h = $r.Bottom - $r.Top
Write-Output "窗口区域(物理像素): ($($r.Left),$($r.Top)) ${w}x${h}"

# PrintWindow 直接向窗口要一份离屏渲染结果，不受其它窗口遮挡影响。
# flags=2 是 PW_RENDERFULLCONTENT，WebView2 这类合成渲染的窗口必须用它，
# 否则会截到一张空白图。
$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
$ok = [WinCap]::PrintWindow($script:hwnd, $hdc, 2)
$g.ReleaseHdc($hdc)
$g.Dispose()
Write-Output "PrintWindow 返回: $ok"

# 产物落进仓库根的 .scratch/（gitignore 的临时区）。用**脚本位置**推仓库根，
# 而不是 Get-Location —— 从哪个目录调用它都能跑对。
$scratch = Join-Path $PSScriptRoot '..\..\.scratch'
New-Item -ItemType Directory -Path $scratch -Force | Out-Null
$out = Join-Path $scratch 'app-window.png'
$bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Output "已保存: $out"

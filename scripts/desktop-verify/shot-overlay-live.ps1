# 在同一个进程里完成「唤起浮层 → 立刻抓屏」，避免两次命令之间浮层失焦自动收起。
#
# 用屏幕合成结果（CopyFromScreen）而不是 PrintWindow，因为要验证的是
# transparent:true 到底有没有生效 —— PrintWindow 抓不到分层窗口的 alpha，
# 它会把透明区域报成不透明纯黑，得不出结论。

param(
  [int]$WaitMs = 1300
)

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class Ov {
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, IntPtr extra);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }

  public static void Down(byte vk) { keybd_event(vk, 0, 0, IntPtr.Zero); }
  public static void Up(byte vk)   { keybd_event(vk, 0, 2, IntPtr.Zero); }
  public static string FgTitle() {
    StringBuilder sb = new StringBuilder(512);
    GetWindowText(GetForegroundWindow(), sb, 512);
    return sb.ToString();
  }
}
"@
[void][Ov]::SetProcessDPIAware()

Write-Output "唤起前前台: '$([Ov]::FgTitle())'"

# Ctrl+Shift+Space
[Ov]::Down(0x11); [Ov]::Down(0x10); [Ov]::Down(0x20)
Start-Sleep -Milliseconds 60
[Ov]::Up(0x20); [Ov]::Up(0x10); [Ov]::Up(0x11)
Start-Sleep -Milliseconds $WaitMs

$fg = [Ov]::FgTitle()
Write-Output "唤起后前台: '$fg'"
if ($fg -ne "快速记录") { Write-Output "浮层未取得焦点，中止"; exit 2 }

$r = New-Object Ov+RECT
[void][Ov]::GetWindowRect([Ov]::GetForegroundWindow(), [ref]$r)
$w = $r.Right - $r.Left
$h = $r.Bottom - $r.Top
Write-Output "浮层区域: ($($r.Left),$($r.Top)) ${w}x${h}"

$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($r.Left, $r.Top, 0, 0, (New-Object System.Drawing.Size $w, $h))
$g.Dispose()

# 同 capture.ps1：产物落进仓库根的 .scratch/，路径由脚本位置推出来
$scratch = Join-Path $PSScriptRoot '..\..\.scratch'
New-Item -ItemType Directory -Path $scratch -Force | Out-Null
$out = Join-Path $scratch 'overlay-live.png'
$bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)

Write-Output ""
Write-Output "=== 角落像素（若透明生效，这里应是浮层背后的桌面内容，而不是纯白/纯黑）==="
foreach ($pt in @(@(2,2), @(677,2), @(2,106), @(677,106), @(340,54))) {
  $c = $bmp.GetPixel($pt[0], $pt[1])
  Write-Output ("  ({0,3},{1,3}) #{2:X2}{3:X2}{4:X2}" -f $pt[0], $pt[1], $c.R, $c.G, $c.B)
}
$bmp.Dispose()
Write-Output "已保存: $out"

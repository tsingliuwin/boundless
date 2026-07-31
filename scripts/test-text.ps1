param([string]$HwndStr = "")
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class U {
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(int f, int dx, int dy, int d, int e);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string c, string t);
  [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint m, UIntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
}
"@
if ($HwndStr -ne "") {
  $h = [IntPtr]::new([int64]$HwndStr)
} else {
  $h = [IntPtr]::Zero
  for ($i = 0; $i -lt 20; $i++) {
    $h = [U]::FindWindowW("", "Boundless")
    if ($h -ne [IntPtr]::Zero) { break }
    Start-Sleep -Milliseconds 300
  }
}
if ($h -eq [IntPtr]::Zero) { Write-Output "no window"; exit }
Write-Output "found window: $h"
[U]::SetForegroundWindow($h) | Out-Null
Start-Sleep -Milliseconds 700
function Click($x, $y) {
  [U]::SetForegroundWindow($h) | Out-Null
  Start-Sleep -Milliseconds 100
  [U]::SetCursorPos($x, $y) | Out-Null
  Start-Sleep -Milliseconds 100
  [U]::mouse_event(2, 0, 0, 0, 0) | Out-Null
  Start-Sleep -Milliseconds 60
  [U]::mouse_event(4, 0, 0, 0, 0) | Out-Null
  Start-Sleep -Milliseconds 200
}
function Key($vk) {
  $w = [UIntPtr]::new([int]$vk)
  [U]::PostMessageW($h, 0x0100, $w, [IntPtr]::Zero) | Out-Null
  Start-Sleep -Milliseconds 30
  [U]::PostMessageW($h, 0x0101, $w, [IntPtr]::Zero) | Out-Null
  Start-Sleep -Milliseconds 150
}
function TypeCh($code) {
  $w = [UIntPtr]::new([int]$code)
  [U]::PostMessageW($h, 0x0100, $w, [IntPtr]::Zero) | Out-Null
  [U]::PostMessageW($h, 0x0102, $w, [IntPtr]::Zero) | Out-Null
  [U]::PostMessageW($h, 0x0101, $w, [IntPtr]::Zero) | Out-Null
  Start-Sleep -Milliseconds 120
}
Key 0x54       # T tool
Click 700 400  # create text box
TypeCh 97        # 'a'
TypeCh 98        # 'b'
Start-Sleep -Milliseconds 300
Key 0x1B       # Esc -> commit + Select
Start-Sleep -Milliseconds 600
Click 700 400  # click text to select
Start-Sleep -Milliseconds 600
Write-Output "done"

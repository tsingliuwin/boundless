param(
    [Parameter(Mandatory=$true)][string]$Action,
    [int]$X = 0,
    [int]$Y = 0,
    [int]$X2 = 0,
    [int]$Y2 = 0,
    [string]$Text = "",
    [string]$Out = "E:\rustproject\boundless\shot.png"
)

Add-Type -AssemblyName System.Windows.Forms,System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public class Win32 {
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hwnd, System.Text.StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(int flags, int dx, int dy, int data, int extra);
    [DllImport("user32.dll", SetLastError=true)] public static extern uint SendInput(uint nInputs, INPUT[] pInputs, int cbSize);
    [StructLayout(LayoutKind.Explicit, Size=40)] public struct INPUT {
        [FieldOffset(0)] public uint type;
        [FieldOffset(8)] public KEYBDINPUT ki;
    }
    [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT {
        public ushort wVk;
        public ushort wScan;
        public uint dwFlags;
        public uint time;
        public System.IntPtr dwExtraInfo;
    }
    public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
}
'@

function Get-BoundlessWindow {
    $p = Get-Process boundless -ErrorAction SilentlyContinue
    if ($p) { return $p.MainWindowHandle }
    return [System.IntPtr]::Zero
}

function Activate {
    $h = Get-BoundlessWindow
    if ($h -ne [System.IntPtr]::Zero) {
        [Win32]::SetForegroundWindow($h) | Out-Null
        Start-Sleep -Milliseconds 400
    }
    $r = New-Object "Win32+RECT"
    [Win32]::GetWindowRect($h, [ref]$r) | Out-Null
    Write-Output ("window: " + $r.Left + "," + $r.Top + " -> " + $r.Right + "," + $r.Bottom)
}

function ClickAt($x, $y) {
    [Win32]::SetCursorPos($x, $y) | Out-Null
    Start-Sleep -Milliseconds 120
    [Win32]::mouse_event(0x0002, 0, 0, 0, 0)
    Start-Sleep -Milliseconds 60
    [Win32]::mouse_event(0x0004, 0, 0, 0, 0)
    Start-Sleep -Milliseconds 300
}

function DragTo($x1, $y1, $x2, $y2) {
    [Win32]::SetCursorPos($x1, $y1) | Out-Null
    Start-Sleep -Milliseconds 120
    [Win32]::mouse_event(0x0002, 0, 0, 0, 0)
    $steps = 12
    for ($i = 1; $i -le $steps; $i++) {
        $cx = $x1 + ($x2 - $x1) * $i / $steps
        $cy = $y1 + ($y2 - $y1) * $i / $steps
        [Win32]::SetCursorPos([int]$cx, [int]$cy) | Out-Null
        Start-Sleep -Milliseconds 25
    }
    [Win32]::mouse_event(0x0004, 0, 0, 0, 0)
    Start-Sleep -Milliseconds 300
}

function Shot($out) {
    $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
    $bmp = New-Object System.Drawing.Bitmap $bounds.Width, $bounds.Height
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
    $bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
    Write-Output "saved $out"
}

switch ($Action) {
    "activate" { Activate }
    "foreground" {
        $fg = [Win32]::GetForegroundWindow()
        $sb = New-Object System.Text.StringBuilder 256
        [Win32]::GetWindowText($fg, $sb, 256) | Out-Null
        Write-Output ("foreground: '" + $sb.ToString() + "' hwnd=" + $fg)
    }
    "click"    { ClickAt $X $Y }
    "drag"     { DragTo $X $Y $X2 $Y2 }
    "type"     { [System.Windows.Forms.SendKeys]::SendWait($Text); Start-Sleep -Milliseconds 300 }
    "utype"    {
        # Inject text as KEYEVENTF_UNICODE (produces real WM_CHAR messages).
        # cbSize is the size of INPUT on x64 (40 bytes), hardcoded to avoid
        # PowerShell's nested-type-name parsing quirk.
        $cb = 40
        foreach ($ch in $Text.ToCharArray()) {
            $down = New-Object Win32.INPUT
            $down.type = 1
            $down.ki.wVk = 0
            $down.ki.wScan = [uint16]$ch
            $down.ki.dwFlags = 0x0004
            $up = New-Object Win32.INPUT
            $up.type = 1
            $up.ki.wVk = 0
            $up.ki.wScan = [uint16]$ch
            $up.ki.dwFlags = 0x0006
            $inputs = ,@($down, $up)
            [Win32]::SendInput(2, $inputs[0], $cb) | Out-Null
            Start-Sleep -Milliseconds 30
        }
        Start-Sleep -Milliseconds 300
    }
    "shot"     { Shot $Out }
}

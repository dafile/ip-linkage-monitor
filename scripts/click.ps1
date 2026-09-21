# Click at window-relative coordinates of a process main window (ASCII only)
param(
    [string]$ProcessName = "ip-linkage-monitor",
    [int]$X = 230,
    [int]$Y = 88
)
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class WinClick {
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, int dx, int dy, uint data, UIntPtr extra);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    public const uint LEFTDOWN = 0x02, LEFTUP = 0x04;
    public static int[] RectOf(IntPtr h) {
        RECT r; GetWindowRect(h, out r);
        return new int[] { r.Left, r.Top, r.Right - r.Left, r.Bottom - r.Top };
    }
    public static void ForceTop(IntPtr h) {
        ShowWindow(h, 9); // SW_RESTORE
        IntPtr topmost = new IntPtr(-1);
        SetWindowPos(h, topmost, 0, 0, 0, 0, 0x0001 | 0x0002 | 0x0040);
    }
    public static void Click(int x, int y) {
        SetCursorPos(x, y);
        System.Threading.Thread.Sleep(120);
        mouse_event(LEFTDOWN, x, y, 0, UIntPtr.Zero);
        System.Threading.Thread.Sleep(60);
        mouse_event(LEFTUP, x, y, 0, UIntPtr.Zero);
    }
}
"@
$p = Get-Process $ProcessName -ErrorAction Stop | Select-Object -First 1
if ($p.MainWindowHandle -eq [IntPtr]::Zero) { throw "main window handle is zero" }
[WinClick]::ForceTop($p.MainWindowHandle)
Start-Sleep -Milliseconds 600
$rect = [WinClick]::RectOf($p.MainWindowHandle)
$absX = $rect[0] + $X; $absY = $rect[1] + $Y
Write-Output ("click at window+({0},{1}) -> screen({2},{3})" -f $X, $Y, $absX, $absY)
[WinClick]::Click($absX, $absY)

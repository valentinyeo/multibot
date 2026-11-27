[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Exe,
    [Parameter(Mandatory)][string]$EvidenceDir,
    [ValidateRange(1, 600)][int]$TimeoutSec = 60
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$EvidenceDir = [IO.Path]::GetFullPath($EvidenceDir)
New-Item -ItemType Directory -Force $EvidenceDir | Out-Null
$steps = [Collections.Generic.List[object]]::new()
$result = [ordered]@{ pass = $false; steps = $steps; error = $null; stderr = ''; finalWorkingSetMB = $null }
$doctor = [ordered]@{ pid = $null; processAlive = $false; exePath = $Exe; sha256 = $null; hwnd = $null }
$session = $null
$profile = $null
$clock = [Diagnostics.Stopwatch]::StartNew()

try {
    if (-not $IsWindows) { throw 'This check requires an interactive Windows desktop.' }
    $Exe = (Resolve-Path -LiteralPath $Exe).Path
    $doctor.exePath = $Exe
    $doctor.sha256 = (Get-FileHash -LiteralPath $Exe -Algorithm SHA256).Hash
    # Refuse older trusted release binaries before they can write to real known folders.
    if (-not [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($Exe)).Contains('MULTIBOT_DATA_DIR')) {
        throw 'This binary predates MULTIBOT_DATA_DIR isolation support; build this ref or use a newer release.'
    }
    Add-Type -AssemblyName System.Drawing
    $references = @('System.Drawing.Common', 'System.Drawing.Primitives', 'System.Collections', 'System.Linq', 'System.ComponentModel.Primitives', 'Microsoft.Win32.Primitives')
    $references += @(Get-ChildItem "$PSHOME/System.Private.Windows.*.dll" | Select-Object -ExpandProperty FullName)
    Add-Type -ReferencedAssemblies $references -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Diagnostics;
using System.Drawing;
using System.Drawing.Imaging;
using System.IO;
using System.Linq;
using System.Runtime.InteropServices;
using System.Text;

public sealed class MultibotUiSession : IDisposable {
    [StructLayout(LayoutKind.Sequential)] struct SECURITY_ATTRIBUTES {
        public int length; public IntPtr descriptor; public int inherit;
    }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] struct STARTUPINFO {
        public int cb; public string reserved, desktop, title;
        public int x, y, width, height, xChars, yChars, fill, flags;
        public short show, reservedSize;
        public IntPtr reserved2, stdin, stdout, stderr;
    }
    [StructLayout(LayoutKind.Sequential)] struct PROCESS_INFORMATION {
        public IntPtr process, thread; public uint pid, tid;
    }
    [StructLayout(LayoutKind.Sequential)] struct JOB_BASIC_LIMIT {
        public long processTime, jobTime; public uint flags;
        public UIntPtr minWorkingSet, maxWorkingSet; public uint activeProcessLimit;
        public UIntPtr affinity; public uint priority, scheduling;
    }
    [StructLayout(LayoutKind.Sequential)] struct IO_COUNTERS {
        public ulong readOps, writeOps, otherOps, readBytes, writeBytes, otherBytes;
    }
    [StructLayout(LayoutKind.Sequential)] struct JOB_EXTENDED_LIMIT {
        public JOB_BASIC_LIMIT basic; public IO_COUNTERS io;
        public UIntPtr processMemory, jobMemory, peakProcessMemory, peakJobMemory;
    }
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] struct MOUSEINPUT {
        public int dx, dy; public uint data, flags, time; public UIntPtr extra;
    }
    [StructLayout(LayoutKind.Sequential)] struct KEYBDINPUT {
        public ushort key, scan; public uint flags, time; public UIntPtr extra;
    }
    [StructLayout(LayoutKind.Explicit)] struct INPUT_UNION {
        [FieldOffset(0)] public MOUSEINPUT mouse;
        [FieldOffset(0)] public KEYBDINPUT keyboard;
    }
    [StructLayout(LayoutKind.Sequential)] struct INPUT { public uint type; public INPUT_UNION data; }
    delegate bool EnumProc(IntPtr hwnd, IntPtr arg);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)] static extern IntPtr CreateJobObject(IntPtr attributes, string name);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool SetInformationJobObject(IntPtr job, int kind, ref JOB_EXTENDED_LIMIT info, uint size);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)] static extern bool CreateProcess(string app, StringBuilder command, IntPtr processAttributes, IntPtr threadAttributes, bool inherit, uint flags, IntPtr environment, string cwd, ref STARTUPINFO startup, out PROCESS_INFORMATION process);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)] static extern IntPtr CreateFile(string path, uint access, uint share, ref SECURITY_ATTRIBUTES attributes, uint creation, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true)] static extern uint ResumeThread(IntPtr thread);
    [DllImport("kernel32.dll")] static extern bool TerminateProcess(IntPtr process, uint code);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr handle, uint timeout);
    [DllImport("kernel32.dll")] static extern bool GetExitCodeProcess(IntPtr process, out uint code);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc callback, IntPtr arg);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")] static extern bool IsIconic(IntPtr hwnd);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowText(IntPtr hwnd, StringBuilder text, int count);
    [DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] static extern bool ShowWindow(IntPtr hwnd, int command);
    [DllImport("user32.dll", SetLastError = true)] static extern uint SendInput(uint count, INPUT[] inputs, int size);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();

    IntPtr job, process;
    public uint Pid { get; private set; }
    public bool Alive { get { return process != IntPtr.Zero && WaitForSingleObject(process, 0) == 258; } }
    public uint ExitCode { get { uint code; GetExitCodeProcess(process, out code); return code; } }
    static void Check(bool ok) { if (!ok) throw new Win32Exception(Marshal.GetLastWin32Error()); }

    public MultibotUiSession(string exe, string cwd, string stdout, string stderr, Dictionary<string,string> environment) {
        IntPtr env = IntPtr.Zero, output = IntPtr.Zero, error = IntPtr.Zero, input = IntPtr.Zero;
        PROCESS_INFORMATION pi = new PROCESS_INFORMATION();
        bool assigned = false;
        try {
            job = CreateJobObject(IntPtr.Zero, null);
            Check(job != IntPtr.Zero);
            var limits = new JOB_EXTENDED_LIMIT();
            limits.basic.flags = 0x2000; // KILL_ON_JOB_CLOSE, with a non-inherited job handle.
            Check(SetInformationJobObject(job, 9, ref limits, (uint)Marshal.SizeOf(limits)));
            var sa = new SECURITY_ATTRIBUTES { length = Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES)), inherit = 1 };
            output = CreateFile(stdout, 0x40000000, 3, ref sa, 2, 0, IntPtr.Zero);
            error = CreateFile(stderr, 0x40000000, 3, ref sa, 2, 0, IntPtr.Zero);
            input = CreateFile("NUL", 0x80000000, 3, ref sa, 3, 0, IntPtr.Zero);
            Check(output != new IntPtr(-1) && error != new IntPtr(-1) && input != new IntPtr(-1));
            string block = string.Join("\0", environment.OrderBy(p => p.Key, StringComparer.OrdinalIgnoreCase).Select(p => p.Key + "=" + p.Value)) + "\0\0";
            env = Marshal.StringToHGlobalUni(block);
            var startup = new STARTUPINFO { cb = Marshal.SizeOf(typeof(STARTUPINFO)), flags = 0x100, stdin = input, stdout = output, stderr = error };
            // Suspend before the first instruction, so no child can escape the job.
            Check(CreateProcess(exe, new StringBuilder("\"" + exe + "\""), IntPtr.Zero, IntPtr.Zero, true, 0x404, env, cwd, ref startup, out pi));
            process = pi.process;
            Pid = pi.pid;
            Check(AssignProcessToJobObject(job, process));
            assigned = true;
            Check(ResumeThread(pi.thread) != uint.MaxValue);
        } catch {
            if (!assigned && process != IntPtr.Zero) TerminateProcess(process, 1);
            Dispose();
            throw;
        } finally {
            if (pi.thread != IntPtr.Zero) CloseHandle(pi.thread);
            foreach (var handle in new[] { output, error, input })
                if (handle != IntPtr.Zero && handle != new IntPtr(-1)) CloseHandle(handle);
            if (env != IntPtr.Zero) Marshal.FreeHGlobal(env);
        }
    }
    public void Dispose() {
        if (job != IntPtr.Zero) { CloseHandle(job); job = IntPtr.Zero; }
        if (process != IntPtr.Zero) {
            if (WaitForSingleObject(process, 5000) == 258) throw new Exception("Job teardown did not stop the owned process.");
            CloseHandle(process); process = IntPtr.Zero;
        }
    }
    public IntPtr FindWindow() {
        IntPtr found = IntPtr.Zero;
        EnumWindows((hwnd, arg) => {
            uint pid; RECT r;
            GetWindowThreadProcessId(hwnd, out pid);
            if (pid == Pid && IsWindowVisible(hwnd) && GetWindowRect(hwnd, out r) && r.Right > r.Left && r.Bottom > r.Top) {
                found = hwnd; return false;
            }
            return true;
        }, IntPtr.Zero);
        return found;
    }
    public RECT Bounds(IntPtr hwnd) {
        uint pid; GetWindowThreadProcessId(hwnd, out pid);
        RECT r;
        if (!Alive || pid != Pid || !IsWindowVisible(hwnd) || !GetWindowRect(hwnd, out r) || r.Right <= r.Left || r.Bottom <= r.Top)
            throw new Exception("Owned visible window is no longer available.");
        return r;
    }
    public string Title(IntPtr hwnd) { var text = new StringBuilder(1024); GetWindowText(hwnd, text, text.Capacity); return text.ToString(); }
    public void Focus(IntPtr hwnd) {
        Bounds(hwnd);
        if (IsIconic(hwnd)) ShowWindow(hwnd, 9);
        SetForegroundWindow(hwnd);
        if (GetForegroundWindow() != hwnd) throw new Exception("Cannot focus the owned window; refusing to send keys elsewhere.");
    }
    public void Keys(IntPtr hwnd, ushort[] keys) {
        Focus(hwnd);
        var inputs = new List<INPUT>();
        foreach (ushort key in keys) inputs.Add(new INPUT { type = 1, data = new INPUT_UNION { keyboard = new KEYBDINPUT { key = key } } });
        foreach (ushort key in keys.Reverse()) inputs.Add(new INPUT { type = 1, data = new INPUT_UNION { keyboard = new KEYBDINPUT { key = key, flags = 2 } } });
        Check(SendInput((uint)inputs.Count, inputs.ToArray(), Marshal.SizeOf(typeof(INPUT))) == inputs.Count);
    }
    // Types text into the focused control as Unicode characters, so a journey can reach a
    // palette command by name without depending on the keyboard layout.
    public void Text(IntPtr hwnd, string text) {
        Focus(hwnd);
        foreach (char character in text) {
            var inputs = new[] {
                new INPUT { type = 1, data = new INPUT_UNION { keyboard = new KEYBDINPUT { scan = character, flags = 4 } } },
                new INPUT { type = 1, data = new INPUT_UNION { keyboard = new KEYBDINPUT { scan = character, flags = 6 } } }
            };
            Check(SendInput(2, inputs, Marshal.SizeOf(typeof(INPUT))) == 2);
        }
    }
    public sealed class CaptureResult {
        public string filename; public int width, height; public bool nonBlank;
        public long changedPixels; public double changedFraction;
    }
    static byte[] Pixels(Bitmap image) {
        var data = image.LockBits(new Rectangle(0, 0, image.Width, image.Height), ImageLockMode.ReadOnly, PixelFormat.Format32bppArgb);
        try {
            var bytes = new byte[image.Width * image.Height * 4];
            for (int y = 0; y < image.Height; y++) Marshal.Copy(IntPtr.Add(data.Scan0, y * data.Stride), bytes, y * image.Width * 4, image.Width * 4);
            return bytes;
        } finally { image.UnlockBits(data); }
    }
    public CaptureResult Capture(IntPtr hwnd, string path, string reference) {
        // PowerShell binds a $null argument to "" for a string parameter, so a missing
        // reference arrives empty rather than null. Treat both the same way.
        if (!string.IsNullOrEmpty(reference) && !File.Exists(reference))
            throw new Exception("Reference screenshot does not exist: " + reference);
        if (string.IsNullOrEmpty(path)) throw new Exception("Capture path is empty.");
        Focus(hwnd);
        RECT r = Bounds(hwnd);
        using (var image = new Bitmap(r.Right - r.Left, r.Bottom - r.Top, PixelFormat.Format32bppArgb)) {
            using (var graphics = Graphics.FromImage(image))
                graphics.CopyFromScreen(r.Left, r.Top, 0, 0, image.Size, CopyPixelOperation.SourceCopy);
            image.Save(path, ImageFormat.Png);
            var pixels = Pixels(image);
            bool varied = false, nonBlack = false;
            for (int i = 0; i < pixels.Length; i += 4) {
                nonBlack |= pixels[i] != 0 || pixels[i+1] != 0 || pixels[i+2] != 0;
                varied |= pixels[i] != pixels[0] || pixels[i+1] != pixels[1] || pixels[i+2] != pixels[2];
            }
            long changed = 0;
            if (!string.IsNullOrEmpty(reference)) {
                using (var previous = new Bitmap(reference)) {
                    if (previous.Size != image.Size) throw new Exception("Window size changed during the journey.");
                    var before = Pixels(previous);
                    for (int i = 0; i < pixels.Length; i += 4)
                        if (pixels[i] != before[i] || pixels[i+1] != before[i+1] || pixels[i+2] != before[i+2]) changed++;
                }
            }
            return new CaptureResult { filename = Path.GetFileName(path), width = image.Width, height = image.Height,
                nonBlank = nonBlack && varied && new FileInfo(path).Length > 0,
                changedPixels = changed, changedFraction = (double)changed / (image.Width * image.Height) };
        }
    }
}
'@
    [MultibotUiSession]::SetProcessDPIAware() | Out-Null
    $profile = Join-Path ([IO.Path]::GetTempPath()) ('multibot-ui-' + [guid]::NewGuid())
    $appdata = Join-Path $profile 'AppData/Roaming'
    $localdata = Join-Path $profile 'AppData/Local'
    $temp = Join-Path $profile 'Temp'
    New-Item -ItemType Directory -Force $appdata, $localdata, $temp | Out-Null
    $childEnv = [Collections.Generic.Dictionary[string,string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($entry in [Environment]::GetEnvironmentVariables().GetEnumerator()) {
        $childEnv[$entry.Key] = $entry.Value
    }
    # Do not pass CI credentials or inherited GPU overrides to the desktop app.
    foreach ($key in @($childEnv.Keys)) {
        if ($key -match '(?i)(TOKEN|SECRET|PASSWORD|CREDENTIAL|PRIVATE_KEY)|^MULTIBOT_GPU_') { $childEnv.Remove($key) | Out-Null }
    }
    $childEnv['APPDATA'] = $appdata
    $childEnv['LOCALAPPDATA'] = $localdata
    $childEnv['MULTIBOT_DATA_DIR'] = Join-Path $appdata 'Multibot'
    $childEnv['USERPROFILE'] = $profile
    $childEnv['HOME'] = $profile
    $childEnv['HOMEDRIVE'] = [IO.Path]::GetPathRoot($profile).TrimEnd('\')
    $childEnv['HOMEPATH'] = $profile.Substring($childEnv['HOMEDRIVE'].Length)
    $childEnv['TEMP'] = $temp
    $childEnv['TMP'] = $temp
    $childEnv['RUST_LOG'] = 'info'
    $stdoutPath = Join-Path $EvidenceDir 'stdout.txt'
    $stderrPath = Join-Path $EvidenceDir 'stderr.txt'
    $session = [MultibotUiSession]::new($Exe, $profile, $stdoutPath, $stderrPath, $childEnv)
    $doctor.pid = $session.Pid
    $hwnd = [IntPtr]::Zero
    $wait = [Diagnostics.Stopwatch]::StartNew()
    while ($wait.Elapsed.TotalSeconds -lt $TimeoutSec) {
        if (-not $session.Alive) { throw "Multibot exited before opening a window (code $($session.ExitCode))." }
        $hwnd = $session.FindWindow()
        if ($hwnd -ne [IntPtr]::Zero) { break }
        Start-Sleep -Milliseconds 200
    }
    if ($hwnd -eq [IntPtr]::Zero) { throw "No visible owned window within $TimeoutSec seconds." }
    $session.Focus($hwnd)
    Start-Sleep -Milliseconds 800
    $bounds = $session.Bounds($hwnd)
    $process = Get-Process -Id $session.Pid
    $doctor.processAlive = $session.Alive
    $doctor.hwnd = ('0x{0:X}' -f $hwnd.ToInt64())
    $doctor.windowTitle = $session.Title($hwnd)
    $doctor.bounds = @{ x = $bounds.Left; y = $bounds.Top; width = $bounds.Right - $bounds.Left; height = $bounds.Bottom - $bounds.Top }
    $doctor.workingSetMB = [math]::Round($process.WorkingSet64 / 1MB, 2)
    $doctor.profile = $profile
    $doctor.environment = @{ APPDATA = $appdata; LOCALAPPDATA = $localdata; USERPROFILE = $profile; MULTIBOT_DATA_DIR = $childEnv['MULTIBOT_DATA_DIR']; gpuOverride = 'none (application default)' }
    $gpuPath = Join-Path $appdata 'Multibot/last-gpu.txt'
    $doctor.gpuReport = if (Test-Path -LiteralPath $gpuPath) { Get-Content -Raw -LiteralPath $gpuPath } else { $null }
    $doctor | ConvertTo-Json -Depth 8 | Set-Content (Join-Path $EvidenceDir 'doctor.json')
    # The application writes this report at startup. Its location proves APPDATA isolation,
    # including for downloaded releases which may predate support for process-local APPDATA.
    if (-not $doctor.gpuReport) { throw 'No GPU report inside the isolated profile; cannot prove isolation or renderer selection.' }
    if ($doctor.gpuReport -notmatch '(?m)^MODE\tauto_integrated\r?$') { throw 'The application did not use its default GPU policy.' }
    $active = @($doctor.gpuReport -split '\r?\n' | Where-Object { $_ -like "ACTIVE`t*" })
    if ($active.Count -ne 1) { throw 'GPU report has no unique active adapter.' }
    $adapter = $active[0] -split "`t"
    if ($adapter.Count -lt 7 -or $adapter[2] -eq '10de') { throw 'Invalid GPU report or NVIDIA adapter selected.' }
    $doctor.activeGpu = @{ luid = $adapter[1]; vendor = $adapter[2]; software = $adapter[5] -eq '1'; name = $adapter[6] }

    # Every journey names all three fields. Set-StrictMode is on, so a missing hashtable
    # key is an error rather than $null.
    $journeys = @(
        @{ name = 'launch'; file = '01-launch.png'; keys = @(); text = ''; dismiss = $false },
        @{ name = 'palette'; file = '02-palette.png'; keys = @(0x11, 0x4B); text = ''; dismiss = $true },
        @{ name = 'settings'; file = '03-settings.png'; keys = @(0x11, 0xBC); text = ''; dismiss = $true },
        @{ name = 'inbox'; file = '04-inbox.png'; keys = @(0x12, 0x49); text = ''; dismiss = $false },
        @{ name = 'shortcuts'; file = '05-shortcuts.png'; keys = @(0x70); text = ''; dismiss = $true },
        # Alt+C opens the built-in terminal pane, then Alt+C again closes it (MBRU-6).
        @{ name = 'terminal'; file = '06-terminal.png'; keys = @(0x12, 0x43); text = ''; dismiss = $false },
        @{ name = 'terminal-close'; file = '07-terminal-close.png'; keys = @(0x12, 0x43); text = ''; dismiss = $false },
        # Ctrl+K -> "Toggle Computer View" opens the lazy pane, and the same row closes it
        # again (MBRU-7). Typed by name so the journey does not depend on a chord the pane
        # deliberately does not claim.
        @{ name = 'computer-view'; file = '08-computer-view.png'; keys = @(0x11, 0x4B); text = 'computer view'; dismiss = $false },
        @{ name = 'computer-view-close'; file = '09-computer-view-close.png'; keys = @(0x11, 0x4B); text = 'computer view'; dismiss = $false }
    )
    $reference = $null
    foreach ($journey in $journeys) {
        $timer = [Diagnostics.Stopwatch]::StartNew()
        $step = [ordered]@{ name = $journey.name; pass = $false; elapsedMs = 0; screenshot = $journey.file; capture = $null; escape = $null; error = $null }
        $steps.Add($step)
        try {
            if ($journey.keys.Count) {
                $session.Keys($hwnd, [ushort[]]$journey.keys)
                Start-Sleep -Milliseconds 600
            }
            if ($journey.text) {
                $session.Text($hwnd, [string]$journey.text)
                Start-Sleep -Milliseconds 500
                # Enter runs the highlighted palette row.
                $session.Keys($hwnd, [ushort[]]@(0x0D))
                Start-Sleep -Milliseconds 900
            }
            $path = Join-Path $EvidenceDir $journey.file
            $capture = $session.Capture($hwnd, $path, $reference)
            $step.capture = $capture
            if (-not $capture.nonBlank) { throw "Blank or empty screenshot: $($journey.file)" }
            # More than cursor/caret noise must change when a shortcut opens a surface.
            if ($null -ne $reference -and $capture.changedFraction -lt 0.001) { throw "Shortcut produced no meaningful visual change: $($journey.name)" }
            $reference = $path
            if ($journey.dismiss) {
                $session.Keys($hwnd, [ushort[]]@(0x1B))
                Start-Sleep -Milliseconds 400
                $dismissedPath = Join-Path $EvidenceDir ($journey.file -replace '\.png$', '-escaped.png')
                $dismissed = $session.Capture($hwnd, $dismissedPath, $path)
                $step.escape = $dismissed
                if (-not $dismissed.nonBlank -or $dismissed.changedFraction -lt 0.001) { throw "Esc did not visibly dismiss $($journey.name)." }
                $reference = $dismissedPath
            }
            if (-not $session.Alive) { throw "Multibot crashed during $($journey.name)." }
            $step.pass = $true
        } catch {
            $step.error = $_.Exception.Message
            throw
        } finally {
            $step.elapsedMs = $timer.ElapsedMilliseconds
        }
    }
    $result.finalWorkingSetMB = [math]::Round((Get-Process -Id $session.Pid).WorkingSet64 / 1MB, 2)
    $result.pass = $true
} catch {
    $result.error = $_.Exception.Message
    Write-Warning $result.error
} finally {
    if ($null -ne $session) {
        $doctor.processAlive = $session.Alive
        $result.exitCodeBeforeTeardown = $session.ExitCode
        if ($session.Alive) {
            try { $result.finalWorkingSetMB = [math]::Round((Get-Process -Id $session.Pid).WorkingSet64 / 1MB, 2) }
            catch { $result.pass = $false; if (-not $result.error) { $result.error = $_.Exception.Message } }
        } else {
            $result.pass = $false
            if (-not $result.error) { $result.error = 'Multibot exited before teardown.' }
        }
        try { $session.Dispose(); $result.teardown = 'job closed; owned process exited' }
        catch { $result.pass = $false; $result.error = $_.Exception.Message }
    }
    try {
        if ($profile -and (Test-Path -LiteralPath $profile)) {
            $profileEvidence = Join-Path $EvidenceDir 'profile'
            New-Item -ItemType Directory -Force $profileEvidence | Out-Null
            Get-ChildItem -LiteralPath $profile -Force | Copy-Item -Destination $profileEvidence -Recurse -Force
            Remove-Item -LiteralPath $profile -Recurse -Force
        }
        $stderr = Join-Path $EvidenceDir 'stderr.txt'
        if (Test-Path -LiteralPath $stderr) { $result.stderr = [IO.File]::ReadAllText($stderr) }
    } catch { $result.pass = $false; $result.error = "Evidence collection failed: $($_.Exception.Message)" }
    $result.elapsedMs = $clock.ElapsedMilliseconds
    $doctor | ConvertTo-Json -Depth 8 | Set-Content (Join-Path $EvidenceDir 'doctor.json')
    $result | ConvertTo-Json -Depth 10 | Set-Content (Join-Path $EvidenceDir 'result.json')
}
if (-not $result.pass) { exit 1 }

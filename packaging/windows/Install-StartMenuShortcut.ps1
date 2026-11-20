<#
.SYNOPSIS
  Adds (or removes) a Start Menu shortcut for the Filewinch in this folder.

.DESCRIPTION
  Creates %APPDATA%\Microsoft\Windows\Start Menu\Programs\Filewinch.lnk pointing at
  Filewinch.exe next to this script, with the AppUserModelID app.xpdt.filewinch set on the
  shortcut. Filewinch gives its process the same id, so Windows groups its taskbar buttons
  under this shortcut's icon and name, and pinning works. Per user: no admin rights needed.

  Run it from the unzipped folder (keep Filewinch.exe there; move the folder first if you want
  it elsewhere, e.g. %LOCALAPPDATA%\Programs\Filewinch, then run this again):
    powershell -ExecutionPolicy Bypass -File .\Install-StartMenuShortcut.ps1
    powershell -ExecutionPolicy Bypass -File .\Install-StartMenuShortcut.ps1 -Uninstall
  (-ExecutionPolicy Bypass because a script from a downloaded zip is blocked by default;
  `Unblock-File .\Install-StartMenuShortcut.ps1` once works too.) PowerShell 5.1 or later.

.PARAMETER Uninstall
  Removes the shortcut instead. The app's files are left alone; delete the folder to remove them.

.PARAMETER ExePath
  The Filewinch.exe the shortcut opens. Defaults to the one next to this script.
#>
[CmdletBinding()]
param(
    [switch]$Uninstall,
    [string]$ExePath = (Join-Path $PSScriptRoot 'Filewinch.exe')
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'

$AppId = 'app.xpdt.filewinch'
$Programs = [Environment]::GetFolderPath('Programs')
if (-not $Programs) { throw 'Could not find the Start Menu Programs folder for this user.' }
$Link = Join-Path $Programs 'Filewinch.lnk'

if ($Uninstall) {
    if (Test-Path -LiteralPath $Link) {
        Remove-Item -LiteralPath $Link -Force
        Write-Host "Removed $Link"
    } else {
        Write-Host "No shortcut to remove at $Link"
    }
    return
}

if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) {
    throw "Filewinch.exe not found at '$ExePath'. Run this script from the folder you unzipped, or pass -ExePath."
}
$ExePath = (Resolve-Path -LiteralPath $ExePath).ProviderPath
$Folder = Split-Path -Parent $ExePath
$Icon = Join-Path $Folder 'filewinch.ico'
if (-not (Test-Path -LiteralPath $Icon -PathType Leaf)) { $Icon = $ExePath }

# IShellLinkW writes the .lnk; its IPropertyStore carries PKEY_AppUserModel_ID, which
# WScript.Shell's shortcut object can't set.
if (-not ('Filewinch.Setup.Shortcut' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;
using System.Text;

namespace Filewinch.Setup
{
    [ComImport, Guid("000214F9-0000-0000-C000-000000000046"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IShellLinkW
    {
        void GetPath([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder pszFile, int cch, IntPtr pfd, uint fFlags);
        void GetIDList(out IntPtr ppidl);
        void SetIDList(IntPtr pidl);
        void GetDescription([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder pszName, int cch);
        void SetDescription([MarshalAs(UnmanagedType.LPWStr)] string pszName);
        void GetWorkingDirectory([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder pszDir, int cch);
        void SetWorkingDirectory([MarshalAs(UnmanagedType.LPWStr)] string pszDir);
        void GetArguments([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder pszArgs, int cch);
        void SetArguments([MarshalAs(UnmanagedType.LPWStr)] string pszArgs);
        void GetHotkey(out short pwHotkey);
        void SetHotkey(short wHotkey);
        void GetShowCmd(out int piShowCmd);
        void SetShowCmd(int iShowCmd);
        void GetIconLocation([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder pszIconPath, int cch, out int piIcon);
        void SetIconLocation([MarshalAs(UnmanagedType.LPWStr)] string pszIconPath, int iIcon);
        void SetRelativePath([MarshalAs(UnmanagedType.LPWStr)] string pszPathRel, uint dwReserved);
        void Resolve(IntPtr hwnd, uint fFlags);
        void SetPath([MarshalAs(UnmanagedType.LPWStr)] string pszFile);
    }

    [ComImport, Guid("886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IPropertyStore
    {
        void GetCount(out uint cProps);
        void GetAt(uint iProp, out PropertyKey pkey);
        void GetValue(ref PropertyKey key, out PropVariant pv);
        void SetValue(ref PropertyKey key, ref PropVariant pv);
        void Commit();
    }

    [ComImport, Guid("00021401-0000-0000-C000-000000000046")]
    class ShellLink { }

    [StructLayout(LayoutKind.Sequential, Pack = 4)]
    struct PropertyKey
    {
        public Guid FormatId;
        public uint PropertyId;
        public PropertyKey(Guid formatId, uint propertyId) { FormatId = formatId; PropertyId = propertyId; }
    }

    // PROPVARIANT: a VARTYPE, three reserved WORDs, then a 16-byte union (24 bytes on x64; the
    // second long only pads it, harmless on x86 where the native struct is 16 bytes).
    [StructLayout(LayoutKind.Explicit)]
    struct PropVariant
    {
        [FieldOffset(0)] public ushort VarType;
        [FieldOffset(8)] public IntPtr Pointer;
        [FieldOffset(8)] public long Union0;
        [FieldOffset(16)] public long Union1;
    }

    public static class Shortcut
    {
        const ushort VT_LPWSTR = 31;
        const int STGM_READ = 0;
        // PKEY_AppUserModel_ID, propkey.h.
        static PropertyKey AppUserModelId = new PropertyKey(new Guid("9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3"), 5);

        [DllImport("ole32.dll")]
        static extern int PropVariantClear(ref PropVariant pvar);

        public static void Create(string linkPath, string target, string workingDir, string iconPath, string description, string appId)
        {
            object link = new ShellLink();
            try
            {
                IShellLinkW shellLink = (IShellLinkW)link;
                shellLink.SetPath(target);
                shellLink.SetWorkingDirectory(workingDir);
                shellLink.SetIconLocation(iconPath, 0);
                shellLink.SetDescription(description);

                IPropertyStore store = (IPropertyStore)link;
                PropVariant value = new PropVariant();
                value.VarType = VT_LPWSTR;
                value.Pointer = Marshal.StringToCoTaskMemUni(appId);
                try
                {
                    store.SetValue(ref AppUserModelId, ref value);
                    store.Commit();
                }
                finally
                {
                    PropVariantClear(ref value);
                }
                ((IPersistFile)link).Save(linkPath, true);
            }
            finally
            {
                Marshal.FinalReleaseComObject(link);
            }
        }

        // The AppUserModelID stored in a .lnk, or null when it has none.
        public static string ReadAppId(string linkPath)
        {
            object link = new ShellLink();
            try
            {
                ((IPersistFile)link).Load(linkPath, STGM_READ);
                PropVariant value;
                ((IPropertyStore)link).GetValue(ref AppUserModelId, out value);
                try
                {
                    return value.VarType == VT_LPWSTR ? Marshal.PtrToStringUni(value.Pointer) : null;
                }
                finally
                {
                    PropVariantClear(ref value);
                }
            }
            finally
            {
                Marshal.FinalReleaseComObject(link);
            }
        }
    }
}
'@
}

if (Test-Path -LiteralPath $Link) {
    Write-Host "Replacing the existing shortcut at $Link"
}
# Write it in %TEMP%, check it, then move it in, so a failure never leaves a half-written shortcut
# in the Start Menu.
$Temp = Join-Path ([IO.Path]::GetTempPath()) ('Filewinch.{0}.lnk' -f [Guid]::NewGuid().ToString('N'))
try {
    [Filewinch.Setup.Shortcut]::Create($Temp, $ExePath, $Folder, $Icon,
        'Browse, transfer and sync files with servers and cloud storage', $AppId)
    $Written = [Filewinch.Setup.Shortcut]::ReadAppId($Temp)
    if ($Written -ne $AppId) {
        throw "The shortcut was written but its AppUserModelID reads back as '$Written', not '$AppId'."
    }
    Move-Item -LiteralPath $Temp -Destination $Link -Force
} finally {
    if (Test-Path -LiteralPath $Temp) { Remove-Item -LiteralPath $Temp -Force }
}
Write-Host "Added $Link -> $ExePath (AppUserModelID $AppId)"

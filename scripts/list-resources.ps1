param(
    [Parameter(Mandatory = $true)]
    [string]$Image
)

# Enumerate the RT_RCDATA resources an image carries, by identifier.
Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Runtime.InteropServices;

public static class Resources {
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr LoadLibraryExW(string file, IntPtr reserved, uint flags);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr FindResourceW(IntPtr module, IntPtr name, IntPtr type);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr LoadResource(IntPtr module, IntPtr resource);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr LockResource(IntPtr loaded);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern uint SizeofResource(IntPtr module, IntPtr resource);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool FreeLibrary(IntPtr module);

    public static List<string> List(string image) {
        const uint LOAD_LIBRARY_AS_DATAFILE = 0x00000002;
        var found = new List<string>();
        IntPtr module = LoadLibraryExW(image, IntPtr.Zero, LOAD_LIBRARY_AS_DATAFILE);
        if (module == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());
        for (int id = 1; id <= 64; id++) {
            IntPtr resource = FindResourceW(module, (IntPtr)id, (IntPtr)10);
            if (resource == IntPtr.Zero) continue;
            found.Add(id + "\t" + SizeofResource(module, resource) + " bytes");
        }
        FreeLibrary(module);
        return found;
    }
}
'@

[Resources]::List((Resolve-Path $Image).Path) | ForEach-Object { $_ }

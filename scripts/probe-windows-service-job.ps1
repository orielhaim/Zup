param(
    [Parameter(Mandatory)][string]$Tools,
    [Parameter(Mandatory)][string]$Result
)
$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
$utf8=[Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding=$utf8
$OutputEncoding=$utf8
Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class NativeJobProbe {
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern IntPtr OpenJobObjectW(uint rights, bool inherit, string name);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool QueryInformationJobObject(IntPtr job, int kind, byte[] output, uint length, out uint returned);
    [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr OpenProcess(uint rights, bool inherit, uint pid);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool IsProcessInJob(IntPtr process, IntPtr job, out bool member);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr handle, uint timeout);
    [DllImport("advapi32.dll", SetLastError=true)] static extern uint GetSecurityInfo(IntPtr handle, int kind, uint parts, out IntPtr owner, out IntPtr group, out IntPtr dacl, out IntPtr sacl, out IntPtr descriptor);
    [DllImport("advapi32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool ConvertSecurityDescriptorToStringSecurityDescriptorW(IntPtr descriptor, uint revision, uint parts, out IntPtr text, out uint length);
    [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr pointer);
    static IntPtr job=IntPtr.Zero;
    static List<IntPtr> processes=new List<IntPtr>();
    static void Check(bool success) { if(!success)throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error()); }
    static byte[] Query(int kind, int length) {
        byte[] output=new byte[length]; uint returned;
        Check(QueryInformationJobObject(job,kind,output,(uint)length,out returned));return output;
    }
    public static void Pin(string name, uint main) {
        job=OpenJobObjectW(0x00120004,false,name);Check(job!=IntPtr.Zero);
        IntPtr process=OpenProcess(0x00101000,false,main);Check(process!=IntPtr.Zero);
        bool member;Check(IsProcessInJob(process,job,out member));CloseHandle(process);
        if(!member)throw new Exception("SCM main process is outside its declared native job");
        byte[] members=Query(3,4096);int count=BitConverter.ToInt32(members,4);
        if(count<1 || count>500)throw new Exception("Unexpected native process census");
        for(int i=0;i<count;i++) {
            uint pid=(uint)BitConverter.ToInt64(members,8+i*8);
            IntPtr handle=OpenProcess(0x00101000,false,pid);Check(handle!=IntPtr.Zero);processes.Add(handle);
        }
    }
    public static uint Active() { return BitConverter.ToUInt32(Query(1,48),40); }
    public static uint Limits() { return BitConverter.ToUInt32(Query(9,144),16); }
    public static string Security() {
        IntPtr owner,group,dacl,sacl,descriptor,text;uint length;
        uint error=GetSecurityInfo(job,6,5,out owner,out group,out dacl,out sacl,out descriptor);
        if(error!=0)throw new System.ComponentModel.Win32Exception((int)error);
        try {
            Check(ConvertSecurityDescriptorToStringSecurityDescriptorW(descriptor,1,5,out text,out length));
            try{return Marshal.PtrToStringUni(text);}finally{LocalFree(text);}
        }finally{LocalFree(descriptor);}
    }
    public static void Release() { if(job!=IntPtr.Zero){CloseHandle(job);job=IntPtr.Zero;} }
    public static bool MembersExited() { foreach(IntPtr handle in processes)if(WaitForSingleObject(handle,15000)!=0)return false;return true; }
    public static void Close() { Release();foreach(IntPtr handle in processes)CloseHandle(handle);processes.Clear(); }
}
"@
$suffix=[Guid]::NewGuid().ToString('N')
$fixture=Join-Path $env:ProgramFiles "ZupJobProbe-$suffix"
New-Item -ItemType Directory -Path $fixture | Out-Null
$installed=[Collections.Generic.List[object]]::new()
$report=@{result='failed';fixture=$fixture;scope='native job capability probe; deployment scope integration remains pending';service_sha256=(Get-FileHash -LiteralPath (Join-Path $Tools 'zup-test-service.exe') -Algorithm SHA256).Hash;checks=@();observations=@()}
function Native([string]$Executable,[string[]]$Arguments) {
    $stdout=Join-Path $fixture 'stdout.txt';$stderr=Join-Path $fixture 'stderr.txt'
    $quoted=$Arguments|ForEach-Object {
        if($_ -match '["\r\n]' -or $_.EndsWith('\')){throw 'Unsupported fixture argument'}
        '"'+$_+'"'
    }
    $process=Start-Process -FilePath $Executable -ArgumentList $quoted -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $null=$process.Handle
    if(-not $process.WaitForExit(240000)){$process.Kill();throw 'Native probe command timed out'}
    $process.WaitForExit()
    $output=[IO.File]::ReadAllText($stdout)
    if($process.ExitCode){throw "Native probe command failed: $output $([IO.File]::ReadAllText($stderr))"}
    $output
}
function Install([string]$Mode,[string]$ProbeJob='') {
    $tag=[Guid]::NewGuid().ToString('N');$app="com.example.zup-job-$tag";$name="zup-job-$tag"
    $root=Join-Path $fixture $tag;New-Item -ItemType Directory -Path "$root/dist" | Out-Null
    Copy-Item -LiteralPath (Join-Path $Tools 'zup-test-service.exe') -Destination "$root/dist/worker.exe"
    $job="Global\zup-job-probe-$tag"
    $extra=if($ProbeJob){", '--probe-job', '$ProbeJob'"}else{''}
    [IO.File]::WriteAllText("$root/zup.toml",@"
schema=1
frontend="headless"
[app]
id="$app"
name="Native job scope probe"
version="1.0.0"
[build.targets.default]
target="windows-x64"
source={directory="dist"}
[install]
scope="machine"
[install.directory]
machine="`${location.programs}/$app"
[[install.data]]
id="state"
path="`${location.shared_data}/$app-data"
access={kind="services",services=["worker"]}
[[files]]
id="worker"
source="worker.exe"
executable=true
[[services]]
id="worker"
native_name="$name"
binary="`${payload}/worker.exe"
arguments=["$name","$Mode","`${data.state}",'--job','$job'$extra]
execution={type="windows_job",account="local_service"}
start="manual"
lifecycle={install="running",change="preserve"}
"@,$utf8)
    $setup="$root/setup.exe"
    $null=Native (Join-Path $Tools 'zup.exe') @('build','--manifest',"$root/zup.toml",'--runtime',(Join-Path $Tools 'zup-setup-headless-windows-x64.exe'),'--output',$setup)
    $null=Native $setup @('install','--scope','machine','--non-interactive','--yes','--output','json')
    $record=@{setup=$setup;name=$name;job=$job;data=(Join-Path $env:ProgramData "$app-data")}
    $installed.Add($record)
    $record
}
function Stop-Owned($Record) {
    $state=Get-CimInstance Win32_Service -Filter "Name='$($Record.name)'"
    $process=Get-Process -Id $state.ProcessId
    $null=$process.Handle
    try {
        Stop-Service -Name $Record.name
        (Get-Service -Name $Record.name).WaitForStatus('Stopped',[TimeSpan]::FromSeconds(20))
        if(-not $process.WaitForExit(20000)){throw 'SCM main process did not exit'}
    }finally{$process.Dispose()}
}
try {
    foreach($mode in @('job-workers','job-orphan')) {
        $source=Install $mode
        $state=Get-CimInstance Win32_Service -Filter "Name='$($source.name)'"
        [NativeJobProbe]::Pin($source.job,$state.ProcessId)
        $flags=[NativeJobProbe]::Limits()
        if(($flags -band 0x2000) -eq 0 -or ($flags -band 0x1800) -ne 0){throw 'Job permits escape or lacks kill-on-close'}
        if([NativeJobProbe]::Active() -lt 2){throw 'Native worker was not contained'}
        $foreign=Install 'job-foreign' $source.job
        if([IO.File]::ReadAllText((Join-Path $foreign.data 'foreign-job-mutation-denied')) -ne 'true'){throw 'A different LocalService service can mutate the scope'}
        Stop-Owned $foreign
        $security=[NativeJobProbe]::Security()
        Stop-Owned $source
        $remaining=[NativeJobProbe]::Active()
        if($mode -eq 'job-workers' -and $remaining -ne 0){throw 'Cooperating service did not drain workers'}
        if($mode -eq 'job-orphan' -and $remaining -eq 0){throw 'Adversarial fixture failed to retain an orphan worker'}
        $report.observations+=@{mode=$mode;scm_stopped=$true;main_exited=$true;remaining=$remaining;limits=$flags;security=$security}
        [NativeJobProbe]::Release()
        if(-not [NativeJobProbe]::MembersExited()){throw 'Closing the final query handle did not terminate all scope members'}
        [NativeJobProbe]::Close()
    }
    $report.checks=@('LocalService creates protected native scope independently','exact main and child membership','foreign same-account service cannot set limits or change DACL','SCM stop/main exit can leave a contained worker alive','query handle preserves scope observation','final handle closure kills remaining members')
    $report.result='passed'
}catch{
    $report.error=$_.Exception.Message
    throw
}finally{
    [NativeJobProbe]::Close()
    foreach($record in $installed) {
        if((Get-Service -Name $record.name -ErrorAction SilentlyContinue).Status -eq 'Running'){Stop-Service -Name $record.name}
        if($report.result -eq 'passed'){$null=Native $record.setup @('uninstall','--scope','machine','--non-interactive','--yes','--output','json')}
    }
    [IO.File]::WriteAllText($Result,($report|ConvertTo-Json -Depth 6),$utf8)
}
if($report.result -ne 'passed'){throw "Native job probe failed; retain $fixture"}

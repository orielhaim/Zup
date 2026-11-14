"""Prepare and manage an isolated Windows Server evaluation guest from WSL."""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import stat
import subprocess
import tempfile
import time
import xml.etree.ElementTree as ET


NS = "urn:schemas-microsoft-com:unattend"
WCM = "http://schemas.microsoft.com/WMIConfig/2002/State"
ET.register_namespace("", NS)
ET.register_namespace("wcm", WCM)


def element(parent, tag, text=None, **attributes):
    node = ET.SubElement(parent, f"{{{NS}}}{tag}", attributes)
    if text is not None:
        node.text = str(text)
    return node


def component(settings, name):
    return element(settings, "component", name=name, processorArchitecture="amd64",
                   publicKeyToken="31bf3856ad364e35", language="neutral", versionScope="nonSxS")


def password(parent, name, value):
    node = element(parent, name)
    element(node, "Value", value)
    element(node, "PlainText", "true")


def command(arguments, **options):
    return subprocess.run(arguments, check=True, capture_output=True, text=True, **options)


def owned_directory(path):
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise RuntimeError(f"VM directory must be private and owned by the executing user: {path}")


def unattended(credentials):
    root = ET.Element(f"{{{NS}}}unattend")
    pe = element(root, "settings", **{"pass": "windowsPE"})
    locale = component(pe, "Microsoft-Windows-International-Core-WinPE")
    element(element(locale, "SetupUILanguage"), "UILanguage", "en-US")
    for key in ("InputLocale", "SystemLocale", "UILanguage", "UserLocale"):
        element(locale, key, "en-US")
    setup = component(pe, "Microsoft-Windows-Setup")
    disk = element(element(setup, "DiskConfiguration"), "Disk", **{f"{{{WCM}}}action": "add"})
    element(disk, "DiskID", 0)
    element(disk, "WillWipeDisk", "true")
    create = element(disk, "CreatePartitions")
    for order, kind, size in ((1, "EFI", 260), (2, "MSR", 16), (3, "Primary", None)):
        partition = element(create, "CreatePartition", **{f"{{{WCM}}}action": "add"})
        element(partition, "Order", order)
        element(partition, "Type", kind)
        element(partition, "Extend" if size is None else "Size", "true" if size is None else size)
    modify = element(disk, "ModifyPartitions")
    for order, number, filesystem, label in ((1, 1, "FAT32", "System"), (2, 3, "NTFS", "Windows")):
        partition = element(modify, "ModifyPartition", **{f"{{{WCM}}}action": "add"})
        for key, value in (("Order", order), ("PartitionID", number), ("Format", filesystem), ("Label", label)):
            element(partition, key, value)
        if number == 3:
            element(partition, "Letter", "C")
    image = element(element(setup, "ImageInstall"), "OSImage")
    metadata = element(element(image, "InstallFrom"), "MetaData", **{f"{{{WCM}}}action": "add"})
    element(metadata, "Key", "/IMAGE/INDEX")
    element(metadata, "Value", 2)
    destination = element(image, "InstallTo")
    element(destination, "DiskID", 0)
    element(destination, "PartitionID", 3)
    element(image, "WillShowUI", "OnError")
    data = element(setup, "UserData")
    element(data, "AcceptEula", "true")
    element(data, "FullName", "Zup verification")
    element(data, "Organization", "Zup verification")
    specialize = element(root, "settings", **{"pass": "specialize"})
    shell = component(specialize, "Microsoft-Windows-Shell-Setup")
    element(shell, "ComputerName", "ZUPVERIFY")
    element(shell, "TimeZone", "UTC")
    oobe = element(root, "settings", **{"pass": "oobeSystem"})
    shell = component(oobe, "Microsoft-Windows-Shell-Setup")
    screens = element(shell, "OOBE")
    for key in ("HideEULAPage", "HideOEMRegistrationScreen", "HideOnlineAccountScreens", "HideWirelessSetupInOOBE"):
        element(screens, key, "true")
    element(screens, "ProtectYourPC", 3)
    users = element(shell, "UserAccounts")
    password(users, "AdministratorPassword", credentials["builtin_password"])
    accounts = element(users, "LocalAccounts")
    for name, group, secret in (("zup-admin", "Administrators", "admin_password"), ("zup-user", "Users", "user_password")):
        account = element(accounts, "LocalAccount", **{f"{{{WCM}}}action": "add"})
        element(account, "Name", name)
        element(account, "DisplayName", name)
        element(account, "Group", group)
        password(account, "Password", credentials[secret])
    login = element(shell, "AutoLogon")
    element(login, "Username", "zup-admin")
    element(login, "Domain", ".")
    element(login, "Enabled", "true")
    element(login, "LogonCount", 1)
    password(login, "Password", credentials["admin_password"])
    first = element(element(shell, "FirstLogonCommands"), "SynchronousCommand", **{f"{{{WCM}}}action": "add"})
    element(first, "Order", 1)
    element(first, "Description", "Configure isolated verification access")
    entry = "$media=Get-CimInstance Win32_LogicalDisk | Where-Object { $_.DriveType -eq 5 -and (Test-Path ($_.DeviceID+'\\ZupVmSetup.ps1')) } | Select-Object -First 1; if(-not $media){throw 'Verification seed is missing'}; & ($media.DeviceID+'\\ZupVmSetup.ps1')"
    encoded = base64.b64encode(entry.encode("utf-16-le")).decode()
    element(first, "CommandLine", "powershell.exe -NoProfile -ExecutionPolicy Bypass -EncodedCommand " + encoded)
    return ET.tostring(root, encoding="utf-8", xml_declaration=True)


SETUP = r"""$ErrorActionPreference = 'Stop'
$root = 'C:\ProgramData\ZupVmVerification'
New-Item -ItemType Directory -Path $root -Force | Out-Null
& icacls.exe $root /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)(F)' '*S-1-5-32-544:(OI)(CI)(F)' | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Cannot protect guest verification state' }
try {
    if (-not (Get-Service sshd -ErrorAction SilentlyContinue)) {
        Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0 | Out-Null
    }
    Stop-Service sshd -ErrorAction SilentlyContinue
    $ssh = Join-Path $env:ProgramData 'ssh'
    New-Item -ItemType Directory -Path $ssh -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'host') -Destination (Join-Path $ssh 'ssh_host_ed25519_key')
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'host.pub') -Destination (Join-Path $ssh 'ssh_host_ed25519_key.pub')
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'client.pub') -Destination (Join-Path $ssh 'administrators_authorized_keys')
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'client.pub') -Destination (Join-Path $ssh 'verification_user_authorized_keys')
    foreach ($file in @('ssh_host_ed25519_key','administrators_authorized_keys','verification_user_authorized_keys')) {
        $path = Join-Path $ssh $file
        & icacls.exe $path /inheritance:r /grant:r '*S-1-5-18:(F)' '*S-1-5-32-544:(F)' | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "Cannot protect $file" }
    }
    $sid = (Get-LocalUser -Name 'zup-user').SID.Value
    & icacls.exe (Join-Path $ssh 'verification_user_authorized_keys') /grant ("*${sid}:(R)") | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Cannot authorize standard verification user' }
    @'
Port 22
HostKey __PROGRAMDATA__/ssh/ssh_host_ed25519_key
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
AllowUsers zup-admin zup-user
Subsystem sftp sftp-server.exe
Match User zup-admin
    AuthorizedKeysFile __PROGRAMDATA__/ssh/administrators_authorized_keys
Match User zup-user
    AuthorizedKeysFile __PROGRAMDATA__/ssh/verification_user_authorized_keys
'@ | Set-Content -LiteralPath (Join-Path $ssh 'sshd_config') -Encoding ascii
    New-Item -Path 'HKLM:\SOFTWARE\OpenSSH' -Force | Out-Null
    New-ItemProperty -Path 'HKLM:\SOFTWARE\OpenSSH' -Name DefaultShell -Value "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -PropertyType String -Force | Out-Null
    New-ItemProperty -Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System' -Name LocalAccountTokenFilterPolicy -Value 1 -PropertyType DWord -Force | Out-Null
    Set-Service sshd -StartupType Automatic
    if (-not (Get-NetFirewallRule -Name ZupVmSsh -ErrorAction SilentlyContinue)) {
        New-NetFirewallRule -Name ZupVmSsh -DisplayName 'Zup VM SSH' -Direction Inbound -Action Allow -Protocol TCP -LocalPort 22 -RemoteAddress 10.0.2.2 | Out-Null
    }
    $logon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty -Path $logon -Name AutoAdminLogon -Value '0'
    Remove-ItemProperty -Path $logon -Name DefaultPassword -ErrorAction SilentlyContinue
    foreach ($answer in @('C:\Windows\Panther\unattend.xml','C:\Windows\Panther\Unattend\unattend.xml','C:\Windows\System32\Sysprep\unattend.xml')) {
        Remove-Item -LiteralPath $answer -Force -ErrorAction SilentlyContinue
    }
    Start-Service sshd
    @{ ready = $true; computer = $env:COMPUTERNAME; build = [Environment]::OSVersion.Version.ToString() } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $root 'ready.json')
} catch {
    $_ | Out-String | Set-Content -LiteralPath (Join-Path $root 'setup-error.txt')
    throw
}
"""


def prepare(options):
    media = options.media.resolve(strict=True)
    with media.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    if digest != options.sha256:
        raise RuntimeError("Windows evaluation media does not match the pinned digest")
    options.cache.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="win-", dir=options.cache)).resolve()
    owned_directory(root)
    seed = root / "seed"
    seed.mkdir(mode=0o700)
    for name in ("client", "host"):
        command(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(root / name)])
    credentials = {name: "Zup9!" + secrets.token_urlsafe(24) for name in ("admin_password", "user_password", "builtin_password")}
    (root / "credentials.json").write_text(json.dumps(credentials))
    (seed / "Autounattend.xml").write_bytes(unattended(credentials))
    (seed / "ZupVmSetup.ps1").write_text(SETUP)
    for name in ("host", "host.pub", "client.pub"):
        shutil.copyfile(root / name, seed / name)
    command(["genisoimage", "-quiet", "-joliet", "-rock", "-volid", "ZUPSEED", "-output", str(root / "seed.iso"), str(seed)])
    command(["qemu-img", "create", "-f", "qcow2", str(root / "system.qcow2"), "100G"])
    shutil.copyfile("/usr/share/OVMF/OVMF_VARS_4M.ms.fd", root / "firmware.fd")
    (root / "tpm").mkdir(mode=0o700)
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    (root / "known_hosts").write_text(f"[127.0.0.1]:{port} {(root / 'host.pub').read_text().strip()}\n")
    (root / "vm.json").write_text(json.dumps({"media": str(media), "sha256": digest, "ssh_port": port}, indent=2))
    print(json.dumps({"root": str(root), "ssh_port": port, "prepared": True}))


def qmp(root, request):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(10)
        connection.connect(str(root / "qmp.sock"))
        stream = connection.makefile("rwb")
        json.loads(stream.readline())
        for item in ({"execute": "qmp_capabilities"}, request):
            stream.write(json.dumps(item).encode() + b"\n")
            stream.flush()
            while True:
                result = json.loads(stream.readline())
                if "return" in result:
                    break
                if "error" in result:
                    raise RuntimeError(result["error"])
        return result["return"]


def start(root):
    owned_directory(root)
    if not os.access("/dev/kvm", os.R_OK | os.W_OK):
        raise RuntimeError("KVM access required; run as the VM owner with supplementary kvm access")
    if len(str(root / "swtpm.sock").encode()) > 100 or "," in str(root):
        raise RuntimeError("VM path exceeds QEMU socket or drive-name constraints")
    configuration = json.loads((root / "vm.json").read_text())
    endpoints = [root / "qmp.sock", root / "swtpm.sock"]
    if any(path.exists() for path in endpoints):
        processes = json.loads((root / "processes.json").read_text())
        for role in ("qemu", "swtpm"):
            pid = processes[role]
            if not isinstance(pid, int) or pid <= 1:
                raise RuntimeError("invalid recorded VM process")
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                continue
            raise RuntimeError(f"recorded {role} process remains present; inspect before restarting")
        for endpoint in endpoints:
            try:
                state = endpoint.lstat()
            except FileNotFoundError:
                continue
            if not stat.S_ISSOCK(state.st_mode) or state.st_uid != os.getuid():
                raise RuntimeError("stale VM endpoint is not an owned socket")
            endpoint.unlink()
    with (root / "swtpm.log").open("a") as log:
        tpm = subprocess.Popen(["swtpm", "socket", "--tpm2", "--tpmstate", f"dir={root}/tpm", "--ctrl", f"type=unixio,path={root}/swtpm.sock", "--terminate"], stdin=subprocess.DEVNULL, stdout=log, stderr=log, close_fds=True, start_new_session=True)
    for _ in range(100):
        if tpm.poll() is not None:
            raise RuntimeError("swtpm exited; inspect swtpm.log")
        if (root / "swtpm.sock").exists():
            break
        time.sleep(0.05)
    else:
        tpm.terminate()
        raise RuntimeError("swtpm did not create its private control socket")
    arguments = ["qemu-system-x86_64", "-machine", "q35,smm=on,accel=kvm", "-cpu", "host", "-smp", "4", "-m", "6144",
                 "-global", "driver=cfi.pflash01,property=secure,value=on",
                 "-drive", "if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.ms.fd",
                 "-drive", f"if=pflash,format=raw,unit=1,file={root}/firmware.fd",
                 "-chardev", f"socket,id=chrtpm,path={root}/swtpm.sock", "-tpmdev", "emulator,id=tpm0,chardev=chrtpm", "-device", "tpm-tis,tpmdev=tpm0",
                 "-drive", f"file={root}/system.qcow2,if=ide,index=0,format=qcow2,cache=none",
                 "-drive", f"file={configuration['media']},if=ide,index=2,media=cdrom,readonly=on,id=install",
                 "-drive", f"file={root}/seed.iso,if=ide,index=3,media=cdrom,readonly=on,id=seed",
                 "-boot", "order=c,once=d,menu=off",
                 "-netdev", f"user,id=net0,hostfwd=tcp:127.0.0.1:{configuration['ssh_port']}-:22", "-device", "e1000e,netdev=net0",
                 "-display", "none", "-vga", "std", "-serial", f"file:{root}/serial.log", "-monitor", "none", "-qmp", f"unix:{root}/qmp.sock,server=on,wait=off"]
    if configuration.get("setup_complete"):
        cleaned = []
        index = 0
        while index < len(arguments):
            if arguments[index] == "-drive" and any(marker in arguments[index + 1] for marker in ("id=install", "id=seed")):
                index += 2
                continue
            cleaned.append("order=c,menu=off" if arguments[index] == "order=c,once=d,menu=off" else arguments[index])
            index += 1
        arguments = cleaned
    (root / "launch.json").write_text(json.dumps(arguments, indent=2))
    try:
        with (root / "qemu.log").open("a") as log:
            process = subprocess.Popen(arguments, stdin=subprocess.DEVNULL, stdout=log, stderr=log, close_fds=True, start_new_session=True)
    except BaseException:
        tpm.terminate()
        raise
    (root / "processes.json").write_text(json.dumps({"qemu": process.pid, "swtpm": tpm.pid}))
    for _ in range(100):
        if process.poll() is not None:
            tpm.terminate()
            raise RuntimeError("QEMU exited; inspect qemu.log")
        if (root / "qmp.sock").exists():
            if not configuration.get("setup_complete"):
                time.sleep(3)
                qmp(root, {"execute": "send-key", "arguments": {"keys": [{"type": "qcode", "data": "spc"}]}})
            print(json.dumps({"root": str(root), "started": True, "ssh_port": configuration["ssh_port"], "state": qmp(root, {"execute": "query-status"})}))
            break
        time.sleep(0.1)
    else:
        process.terminate()
        tpm.terminate()
        raise RuntimeError("QEMU did not create its private QMP endpoint")


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    actions = parser.add_subparsers(dest="action", required=True)
    preparation = actions.add_parser("prepare")
    preparation.add_argument("--media", type=Path, required=True)
    preparation.add_argument("--sha256", required=True)
    preparation.add_argument("--cache", type=Path, default=Path.home() / ".cache/zup-verification/vms")
    for name in ("start", "status", "powerdown", "eject-seed"):
        action = actions.add_parser(name)
        action.add_argument("root", type=Path)
    options = parser.parse_args()
    if options.action == "prepare":
        prepare(options)
    else:
        root = options.root.resolve(strict=True)
        owned_directory(root)
        if options.action == "start":
            start(root)
        else:
            request = {"status": {"execute": "query-status"}, "powerdown": {"execute": "system_powerdown"}, "eject-seed": {"execute": "eject", "arguments": {"device": "seed", "force": True}}}[options.action]
            result = qmp(root, request)
            if options.action == "eject-seed":
                block = qmp(root, {"execute": "query-block"})
                seed = next(item for item in block if item["device"] == "seed")
                if "inserted" in seed:
                    raise RuntimeError("setup media remains attached")
                configuration = json.loads((root / "vm.json").read_text())
                configuration["setup_complete"] = True
                (root / "vm.json").write_text(json.dumps(configuration, indent=2))
            print(json.dumps(result))


if __name__ == "__main__":
    main()

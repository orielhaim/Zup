"""Fail closed on Windows privileged discovery, token elevation, and execution."""
import ctypes
from ctypes import wintypes
import json
from pathlib import Path
import subprocess
import sys
import xml.etree.ElementTree as ET

POLICY = Path(__file__).with_name('windows-privileged-tests.json')
FILTER = 'test(/^filesystem::/) | test(/^machine_authority::tests::/) | test(=recovery::tests::machine_runtime_recovery_source_is_private_and_independent_of_original_overlay)'


def discover(document):
    return {(binary, name): test for binary, suite in document['rust-suites'].items()
            for name, test in suite['testcases'].items()}


def verify_discovery(all_tests, selected, policy):
    expected = {(r['binary'], r['name']) for r in policy}
    ignored = {key for key, test in all_tests.items() if test['ignored']}
    if ignored != expected:
        raise ValueError(f'Ignored inventory changed: missing={expected - ignored}, unreviewed={ignored - expected}')
    required = {(r['binary'], r['name']) for r in policy if r['category'] == 'required'}
    actual = {key for key, test in selected.items() if test['filter-match']['status'] == 'matches'}
    if not required or actual != required:
        raise ValueError(f'Privileged selection mismatch: missing={required - actual}, unexpected={actual - required}')
    return required


def require_elevation(elevated):
    if not elevated:
        raise ValueError('Required privileged coverage unavailable: process TokenElevation is false. Use an elevated isolated Windows runner; CI never prompts for UAC.')


def token_elevated():
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    advapi = ctypes.WinDLL('advapi32', use_last_error=True)
    kernel.GetCurrentProcess.restype = wintypes.HANDLE
    advapi.OpenProcessToken.argtypes = [wintypes.HANDLE, wintypes.DWORD, ctypes.POINTER(wintypes.HANDLE)]
    advapi.GetTokenInformation.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD, ctypes.POINTER(wintypes.DWORD)]
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    token = wintypes.HANDLE()
    if not advapi.OpenProcessToken(kernel.GetCurrentProcess(), 8, ctypes.byref(token)):
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        elevation, size = wintypes.DWORD(), wintypes.DWORD()
        if not advapi.GetTokenInformation(token, 20, ctypes.byref(elevation), ctypes.sizeof(elevation), ctypes.byref(size)):
            raise ctypes.WinError(ctypes.get_last_error())
        return elevation.value != 0
    finally:
        kernel.CloseHandle(token)


def verify_report(root, required):
    seen = []
    for suite in root.iter('testsuite'):
        for case in suite.findall('testcase'):
            key = (suite.get('name'), case.get('name'))
            if any(case.find(tag) is not None for tag in ('skipped', 'failure', 'error')):
                raise ValueError(f'Privileged test did not pass: {key}')
            seen.append(key)
    if len(seen) != len(set(seen)) or set(seen) != required:
        raise ValueError(f'Execution evidence mismatch: expected={required}, observed={seen}')
    for binary, name in sorted(required):
        print(f'PASS {binary}::{name}', flush=True)
    print(f'Proved execution of {len(seen)} required privileged tests.', flush=True)


def main():
    base = ['cargo', 'nextest']
    args = ['-p', 'zup-windows', '--all-features', '--run-ignored', 'only']
    evidence = Path('target/f15')
    evidence.mkdir(parents=True, exist_ok=True)
    def listing(name, extra):
        output = subprocess.check_output(base + ['list'] + args + ['--message-format', 'json'] + extra)
        (evidence / name).write_bytes(output)
        return discover(json.loads(output))
    required = verify_discovery(listing('all.json', []), listing('selected.json', ['-E', FILTER]), json.loads(POLICY.read_text()))
    print(f'Discovered {len(required)} required tests; all features enabled.', flush=True)
    require_elevation(token_elevated())
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--format-version', '1']))
    report = Path(metadata['target_directory']) / 'nextest/privileged/junit.xml'
    report.unlink(missing_ok=True)
    result = subprocess.run(base + ['run'] + args + ['--profile', 'privileged', '--no-fail-fast', '--no-tests', 'fail', '-E', FILTER])
    print(f'nextest exit status: {result.returncode}', flush=True)
    if report.exists():
        (evidence / 'junit.xml').write_bytes(report.read_bytes())
        verify_report(ET.parse(report).getroot(), required)
    else:
        raise ValueError('Missing privileged execution report')
    if result.returncode:
        raise ValueError(f'nextest failed with exit status {result.returncode}')


if __name__ == '__main__':
    main()

"""Publish Nextest failures as GitHub annotations without requiring log downloads."""

from pathlib import Path
import sys
import xml.etree.ElementTree as ET


def escape(value):
    return value.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def report_failures(root):
    for report in sorted(root.glob("*/junit.xml")):
        try:
            cases = ET.parse(report).iter("testcase")
        except (OSError, ET.ParseError) as error:
            print(f"::warning::{escape(f'Cannot read {report}: {error}')}")
            continue
        for case in cases:
            failure = case.find("failure")
            if failure is None:
                failure = case.find("error")
            if failure is None:
                continue
            name = f"{case.get('classname', '')}::{case.get('name', '')}"
            detail = "\n".join(
                value
                for value in (
                    name,
                    failure.get("message"),
                    failure.text,
                    case.findtext("system-out"),
                    case.findtext("system-err"),
                )
                if value
            )
            if len(detail) > 16000:
                detail = detail[:16000] + "\n[diagnostic truncated; see test log]"
            print(f"::error::{escape(detail)}")

if __name__ == "__main__":
    sys.stdout.reconfigure(encoding="utf-8", errors="backslashreplace")
    sys.stderr.reconfigure(encoding="utf-8", errors="backslashreplace")
    report_failures(Path("target/nextest"))

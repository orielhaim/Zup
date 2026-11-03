"""Publish Nextest failures as GitHub annotations without requiring log downloads."""

from pathlib import Path
import xml.etree.ElementTree as ET


def escape(value):
    return value.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


for report in sorted(Path("target/nextest").glob("*/junit.xml")):
    for case in ET.parse(report).iter("testcase"):
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

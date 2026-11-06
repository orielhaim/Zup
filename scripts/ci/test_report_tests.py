import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


class ReporterTests(unittest.TestCase):
    def test_cp1252_output_preserves_unicode_and_other_reports_survive_bad_xml(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            reports = root / "target/nextest"
            (reports / "a-broken").mkdir(parents=True)
            (reports / "a-broken/junit.xml").write_text("<broken", encoding="utf-8")
            (reports / "b-valid").mkdir()
            (reports / "b-valid/junit.xml").write_text(
                '<testsuite><testcase classname="π" name="✗">'
                '<failure message="failed">Unicode ✓ שלום 100%\nsecond line</failure>'
                '<system-err>stderr 漢字</system-err></testcase></testsuite>',
                encoding="utf-8",
            )
            result = subprocess.run(
                [sys.executable, str(Path(__file__).with_name("report-tests.py"))],
                cwd=root,
                env={**os.environ, "PYTHONIOENCODING": "cp1252", "PYTHONUTF8": "0"},
                capture_output=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            output = result.stdout.decode("utf-8")
            self.assertIn("::warning::Cannot read", output)
            self.assertIn("::error::π::✗%0Afailed%0AUnicode ✓ שלום 100%25%0Asecond line", output)
            self.assertIn("stderr 漢字", output)


if __name__ == "__main__":
    unittest.main()

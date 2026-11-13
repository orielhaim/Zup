import contextlib
import io
import copy
import json
from pathlib import Path
import unittest
import xml.etree.ElementTree as ET
from verify_windows_privileged import discover, verify_discovery, verify_report, require_elevation, POLICY


class PrivilegedCoverageTests(unittest.TestCase):
    def setUp(self):
        self.policy = json.loads(POLICY.read_text())
        self.tests = {(r['binary'], r['name']): {'ignored': True, 'filter-match': {'status': 'matches' if r['category'] == 'required' else 'mismatch'}} for r in self.policy}
        self.required = verify_discovery(self.tests, self.tests, self.policy)

    def test_discovery_rejects_rename_feature_loss_new_ignored_and_empty(self):
        for mutation in ('rename', 'feature-loss', 'new-ignored', 'empty'):
            with self.subTest(mutation=mutation):
                tests = copy.deepcopy(self.tests)
                key = next(iter(self.required))
                if mutation == 'rename':
                    tests[(key[0], 'moved::' + key[1])] = tests.pop(key)
                elif mutation == 'feature-loss':
                    del tests[key]
                elif mutation == 'new-ignored':
                    tests[('zup-windows', 'new::privileged')] = tests[key]
                else:
                    tests = {}
                with self.assertRaises(ValueError):
                    verify_discovery(tests, tests, self.policy)

    def test_real_stale_filter_discovery_is_rejected(self):
        fixture = json.loads(Path(__file__).with_name('windows-stale-discovery.json').read_text())
        with self.assertRaises(ValueError):
            verify_discovery(self.tests, discover(fixture), self.policy)

    def test_non_elevated_token_is_rejected(self):
        require_elevation(True)
        with self.assertRaises(ValueError):
            require_elevation(False)

    def test_execution_requires_each_test_once_without_skips_or_failures(self):
        root = ET.Element('testsuites')
        suite = ET.SubElement(root, 'testsuite', name='zup-windows')
        for _, name in sorted(self.required):
            ET.SubElement(suite, 'testcase', name=name)
        with contextlib.redirect_stdout(io.StringIO()):
            verify_report(root, self.required)
        for tag in ('skipped', 'failure', 'error'):
            bad = copy.deepcopy(root)
            ET.SubElement(bad.find('.//testcase'), tag)
            with self.assertRaises(ValueError):
                verify_report(bad, self.required)
        suite.remove(suite[0])
        with self.assertRaises(ValueError):
            verify_report(root, self.required)

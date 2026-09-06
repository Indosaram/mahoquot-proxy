import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


SCRIPTS = ("run-task-17-gates.py", "task-17-qa-driver.py")
SOURCE = Path(__file__).resolve().parents[1]


class QaPathTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="qa paths ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.proxy = self.root / "proxy renamed"
        self.desktop = self.root / "desktop renamed"
        for root, member, name in (
            (self.proxy, "crates/gateway", "mahoquot-gateway"),
            (self.desktop, "crates/monitor-ui", "mahoquot-monitor-ui"),
        ):
            (root / member).mkdir(parents=True)
            (root / "Cargo.toml").write_text(f'[workspace]\nmembers = ["{member}"]\n')
            (root / member / "Cargo.toml").write_text(f'[package]\nname = "{name}"\n')
        (self.desktop / "crates/monitor-ui/frontend").mkdir()
        (self.proxy / "scripts").mkdir()
        for script in SCRIPTS:
            shutil.copy2(SOURCE / script, self.proxy / "scripts" / script)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.log = self.root / "subprocess.log"
        for name in ("cargo", "bun", "bash", "date", "curl"):
            stub = self.bin / name
            stub.write_text('#!/bin/sh\nprintf "%s\\n" "$0" >> "$QA_LOG"\nexit 97\n')
            stub.chmod(0o755)
        self.env = {**os.environ, "PATH": str(self.bin), "QA_LOG": str(self.log),
                    "HOME": str(self.root / "home")}
        self.env.pop("MAHOQUOT_DESKTOP_DIR", None)

    def cli(self, script, *args):
        result = subprocess.run(
            [sys.executable, str(self.proxy / "scripts" / script), "--dry-run", *args],
            cwd=self.root, env=self.env, text=True, capture_output=True, timeout=10,
        )
        self.assertFalse(self.log.exists(), result.stdout + result.stderr)
        self.assertFalse(list(self.root.rglob(".omo")))
        return result

    def assert_plan(self, script, desktop, *args):
        result = self.cli(script, *args)
        self.assertEqual(result.returncode, 0, result.stderr)
        plan = json.loads(result.stdout)
        self.assertEqual(plan["proxy_root"], str(self.proxy))
        self.assertEqual(plan["desktop_root"], str(desktop))
        filename = "task-17-gates.txt" if script == SCRIPTS[0] else "task-17-live-surface.http"
        self.assertEqual(plan["evidence"], [str(r / ".omo/evidence/model-registry" / filename)
                                            for r in (self.proxy, desktop)])
        if script == SCRIPTS[0]:
            self.assertEqual(len(plan["gates"]), 8)
            self.assertEqual([g["cwd"] for g in plan["gates"]],
                             [str(self.proxy)] * 2 + [str(desktop)] * 2 +
                             [str(desktop / "crates/monitor-ui/frontend")] * 4)
            self.assertEqual(plan["gates"][0]["cmd"], ["cargo", "test", "--workspace"])
        else:
            for key, suffix in {
                "gateway_bin": "target/debug/mahoquot-gateway",
                "base_catalog": "crates/registry/catalog/models-v1.json",
                "test_key": "tests/fixtures/test-ed25519.key",
                "test_pub": "tests/fixtures/test-ed25519.pub",
            }.items():
                self.assertEqual(plan[key], str(self.proxy / suffix))

    def test_relocated_scripts_dry_run_only_validated_roots(self):
        for script in SCRIPTS:
            with self.subTest(script=script):
                self.assert_plan(script, self.desktop, "--desktop-root", str(self.desktop))

    def test_environment_root_and_cli_precedence(self):
        self.env["MAHOQUOT_DESKTOP_DIR"] = str(self.desktop)
        for script in SCRIPTS:
            with self.subTest(script=script):
                self.assert_plan(script, self.desktop)
                self.env["MAHOQUOT_DESKTOP_DIR"] = str(self.root / "missing")
                self.assert_plan(script, self.desktop, "--desktop-root", "desktop renamed")
                self.env["MAHOQUOT_DESKTOP_DIR"] = str(self.desktop)

    def test_validated_sibling_default(self):
        sibling = self.root / "mahoquot"
        self.desktop.rename(sibling)
        for script in SCRIPTS:
            with self.subTest(script=script):
                self.assert_plan(script, sibling)

    def test_invalid_desktop_rejected_before_execution_or_writes(self):
        manifest = self.desktop / "crates/monitor-ui/Cargo.toml"
        for content in ('[package]\nname = "wrong"\n', 'not valid TOML = [', ''):
            manifest.write_text(content)
            for script in SCRIPTS:
                with self.subTest(script=script, content=content):
                    result = self.cli(script, "--desktop-root", str(self.desktop))
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn("invalid desktop workspace", result.stderr)

    def test_invalid_proxy_rejected_before_execution_or_writes(self):
        (self.proxy / "Cargo.toml").write_text('[workspace]\nmembers = []\n')
        for script in SCRIPTS:
            with self.subTest(script=script):
                result = self.cli(script, "--desktop-root", str(self.desktop))
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn("invalid proxy workspace", result.stderr)


if __name__ == "__main__":
    unittest.main()

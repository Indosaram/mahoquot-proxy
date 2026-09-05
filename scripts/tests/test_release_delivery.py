import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = Path(os.environ.get("RELEASE_DELIVERY_SCRIPTS", ROOT / "scripts"))
TOOL = ROOT / "target/debug/mahoquot-model-catalog"


class ReleaseDeliveryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="release-delivery-")
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name)
        self.env = os.environ.copy()
        self.env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
                        GIT_AUTHOR_NAME="Fixture", GIT_AUTHOR_EMAIL="fixture@example.invalid",
                        GIT_COMMITTER_NAME="Fixture", GIT_COMMITTER_EMAIL="fixture@example.invalid")

    def run_cmd(self, *args, cwd=None, ok=True):
        result = subprocess.run(args, cwd=cwd or self.work, env=self.env,
                                capture_output=True, text=True, timeout=60)
        if ok:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def test_r29_windows_and_unix_hash_only_created_archive(self):
        for platform, target, suffix in [("Windows", "x86_64-pc-windows-msvc", ".exe"),
                                          ("Linux", "x86_64-unknown-linux-gnu", "")]:
            with self.subTest(platform=platform):
                work = self.work / platform
                binary = work / "target" / target / "release" / ("mahoquot-gateway" + suffix)
                binary.parent.mkdir(parents=True)
                marker = b"release fixture executable\n"
                binary.write_bytes(marker)
                binary.chmod(0o755)
                output = work / "outputs"
                self.env["GITHUB_OUTPUT"] = str(output)
                self.run_cmd("bash", str(SCRIPTS / "package-release.sh"), target, platform, cwd=work)
                extension = ".zip" if suffix else ".tar.gz"
                archive = work / ("mahoquot-gateway-" + target + extension)
                digest, name = archive.with_name(archive.name + ".sha256").read_text().split()
                self.assertEqual(name, archive.name)
                self.assertEqual(digest, hashlib.sha256(archive.read_bytes()).hexdigest())
                self.assertEqual(list(work.glob("*.sha256")), [archive.with_name(archive.name + ".sha256")])
                if suffix:
                    with zipfile.ZipFile(archive) as packed:
                        self.assertEqual(packed.namelist(), [binary.name])
                        self.assertEqual(packed.read(binary.name), marker)
                else:
                    with tarfile.open(archive) as packed:
                        self.assertEqual(packed.getnames(), [binary.name])
                        self.assertEqual(packed.extractfile(binary.name).read(), marker)
                self.assertEqual(dict(line.split("=", 1) for line in output.read_text().splitlines()),
                                 {"archive": archive.name, "checksum": archive.name + ".sha256"})

    def test_r29_missing_binary_fails(self):
        self.assertNotEqual(self.run_cmd("bash", str(SCRIPTS / "package-release.sh"),
                                        "missing-target", "Linux", ok=False).returncode, 0)
        self.assertEqual(list(self.work.glob("*.sha256")), [])

    def test_r32_publish_preserves_parent_command_without_git_writes(self):
        calls = self.work / "git-calls.jsonl"
        fakebin = self.work / "bin"
        fakebin.mkdir()
        fakegit = fakebin / "git"
        fakegit.write_text("""#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
with open(os.environ['GIT_CALLS'], 'a') as log:
    log.write(json.dumps(args) + '\\n')
if args[0] == 'ls-remote' and os.environ['HAS_PARENT'] == '1':
    print('a' * 40 + '\\trefs/heads/model-catalog-v1')
elif args[0] == 'rev-parse':
    print('a' * 40)
elif args[0] == 'show':
    print(json.dumps({'version': 0}))
elif args[0] == 'hash-object':
    print('d' * 40)
elif args[0] == 'write-tree':
    print('b' * 40)
elif 'commit-tree' in args:
    print('c' * 40)
""")
        fakegit.chmod(0o755)
        self.env.update(PATH=str(fakebin) + os.pathsep + self.env['PATH'],
                        GIT_CALLS=str(calls), TMPDIR=str(self.work),
                        MAHOQUOT_MODEL_CATALOG_ED25519_PRIVATE_KEY=(
                            ROOT / 'tests/fixtures/test-ed25519.key').read_text().strip())
        for has_parent in ('0', '1'):
            with self.subTest(has_parent=has_parent):
                calls.write_text('')
                self.env['HAS_PARENT'] = has_parent
                self.run_cmd('bash', str(SCRIPTS / 'publish-catalog.sh'), str(TOOL),
                             str(ROOT / 'crates/registry/catalog/models-v1.json'),
                             'test-ed25519-v1', '--key-file',
                             str(ROOT / 'tests/fixtures/test-ed25519.pub'), '--key-id',
                             'test-ed25519-v1', cwd=self.work)
                commands = [json.loads(line) for line in calls.read_text().splitlines()]
                commit = next(args for args in commands if 'commit-tree' in args)
                if has_parent == '1':
                    self.assertIn(['fetch', '--no-tags', 'origin', 'refs/heads/model-catalog-v1'], commands)
                    self.assertEqual(commit[commit.index('-p') + 1], 'a' * 40)
                else:
                    self.assertNotIn('-p', commit)
                push = next(args for args in commands if args[0] == 'push')
                self.assertEqual(push, ['push', 'origin', 'c' * 40 + ':refs/heads/model-catalog-v1'])
                added = [args[-1].split(',')[-1] for args in commands if args[0] == 'update-index']
                self.assertEqual(added, ['models-v1.json', 'models-v1.json.sig', 'models-v1.json.sha256'])
                self.assertFalse(any(self.work.glob('catalog-publish.*')))

    def setup_publisher(self):
        self.remote = self.work / "remote.git"
        self.run_cmd("git", "init", "--bare", str(self.remote))
        self.repo = self.work / "publisher"
        self.run_cmd("git", "init", str(self.repo))
        self.run_cmd("git", "remote", "add", "origin", str(self.remote), cwd=self.repo)
        self.input = self.work / "incoming.json"
        self.env["MAHOQUOT_MODEL_CATALOG_ED25519_PRIVATE_KEY"] = (
            ROOT / "tests/fixtures/test-ed25519.key").read_text().strip()
        self.env["TMPDIR"] = str(self.work)

    def publish(self, version, ok=True):
        catalog = json.loads((ROOT / "crates/registry/catalog/models-v1.json").read_text())
        catalog["version"] = version
        self.input.write_text(json.dumps(catalog))
        return self.run_cmd("bash", str(SCRIPTS / "publish-catalog.sh"), str(TOOL),
                            str(self.input), "test-ed25519-v1", "--key-file",
                            str(ROOT / "tests/fixtures/test-ed25519.pub"),
                            "--key-id", "test-ed25519-v1", cwd=self.repo, ok=ok)

    def tip(self):
        return self.run_cmd("git", "rev-parse", "refs/heads/model-catalog-v1", cwd=self.remote).stdout.strip()

    def test_r32_second_publish_parents_remote_tip_and_rejects_stale_version(self):
        self.setup_publisher()
        self.publish(100)
        first = self.tip()
        self.repo = self.work / "second-publisher"
        self.run_cmd("git", "init", str(self.repo))
        self.run_cmd("git", "remote", "add", "origin", str(self.remote), cwd=self.repo)
        self.publish(101)
        second = self.tip()
        self.assertEqual(self.run_cmd("git", "rev-parse", second + "^", cwd=self.remote).stdout.strip(), first)
        for name in ["models-v1.json", "models-v1.json.sig", "models-v1.json.sha256"]:
            (self.work / name).write_text(self.run_cmd("git", "show", second + ":" + name,
                                                     cwd=self.remote).stdout)
        payload = self.work / "models-v1.json"
        # git show's captured text preserves the canonical payload bytes.
        self.assertEqual(json.loads(payload.read_text())["version"], 101)
        self.assertEqual((self.work / "models-v1.json.sha256").read_text().split()[0],
                         hashlib.sha256(payload.read_bytes()).hexdigest())
        self.run_cmd(str(TOOL), "verify", "--input", str(payload), "--signature",
                     str(self.work / "models-v1.json.sig"), "--key-file",
                     str(ROOT / "tests/fixtures/test-ed25519.pub"), "--key-id", "test-ed25519-v1")
        self.assertNotEqual(self.publish(101, ok=False).returncode, 0)
        self.assertEqual(self.tip(), second)
        self.assertEqual(list(self.work.glob("catalog-publish.*")), [])

    def test_r32_unreachable_remote_is_not_first_publication(self):
        self.setup_publisher()
        self.run_cmd("git", "remote", "set-url", "origin", str(self.work / "absent.git"), cwd=self.repo)
        self.assertNotEqual(self.publish(100, ok=False).returncode, 0)
        self.assertEqual(self.run_cmd("git", "show-ref", cwd=self.repo, ok=False).returncode, 1)


if __name__ == "__main__":
    unittest.main()

import os
import shutil
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


class AutoReleaseTests(unittest.TestCase):
    def test_pushes_one_version_commit_and_tag_and_is_idempotent(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            remote, repo = root / "remote.git", root / "repo"

            def git(*args):
                return subprocess.run(["git", *args], cwd=repo, check=True, text=True,
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE).stdout.strip()

            subprocess.run(["git", "init", "--bare", "--quiet", str(remote)], check=True)
            subprocess.run(["git", "init", "--quiet", "-b", "main", str(repo)], check=True)
            git("config", "user.name", "Test")
            git("config", "user.email", "test@example.com")
            git("remote", "add", "origin", str(remote))
            (repo / "scripts").mkdir()
            shutil.copy(ROOT / "scripts/bump-release-version.py", repo / "scripts")
            shutil.copy(ROOT / "Cargo.lock", repo)
            for manifest in (ROOT / "crates").glob("*/Cargo.toml"):
                dest = repo / "crates" / manifest.parent.name
                dest.mkdir(parents=True)
                shutil.copy(manifest, dest)
            git("add", ".")
            git("commit", "-qm", "Initial release")
            # The script only supports matching workspace versions and tags.
            import tomllib
            version = tomllib.loads((repo / "crates/ninox-core/Cargo.toml").read_text())["package"]["version"]
            git("tag", "v" + version)
            git("push", "-q", "origin", "main", "--tags")
            (repo / "change.txt").write_text("merged work\n")
            git("add", "change.txt")
            git("commit", "-qm", "Merge work")
            git("push", "-q", "origin", "main")
            event = git("rev-parse", "HEAD")

            workflow = (ROOT / ".github/workflows/auto-release.yml").read_text()
            shell = textwrap.dedent(workflow.split("        run: |\n", 1)[1])
            env = {**os.environ, "EVENT_SHA": event}
            for _ in range(2):
                result = subprocess.run(["bash", "-e", "-c", shell], cwd=repo, env=env,
                                        text=True, capture_output=True)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            major, minor, patch = map(int, version.split("."))
            tag = f"v{major}.{minor}.{patch + 1}"
            self.assertEqual(set(git("tag", "--list", "v*").splitlines()), {f"v{version}", tag})
            self.assertEqual(git("rev-parse", "HEAD"), git("rev-parse", tag))
            self.assertEqual(git("log", "-1", "--format=%s"), f"chore(release): {tag}")
            self.assertEqual(git("ls-remote", "origin", "refs/heads/main").split()[0], git("rev-parse", tag))


if __name__ == "__main__":
    unittest.main()

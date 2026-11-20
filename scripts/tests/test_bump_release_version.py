import shutil
import re
import subprocess
import tempfile
import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


class BumpReleaseVersionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "scripts").mkdir()
        shutil.copy(ROOT / "scripts/bump-release-version.py", self.root / "scripts")
        shutil.copy(ROOT / "Cargo.lock", self.root)
        for manifest in (ROOT / "crates").glob("*/Cargo.toml"):
            dest = self.root / "crates" / manifest.parent.name
            dest.mkdir(parents=True)
            shutil.copy(manifest, dest)

    def run_bump(self):
        return subprocess.run(["python3", str(self.root / "scripts/bump-release-version.py")],
                              text=True, capture_output=True)

    def test_bumps_packages_internal_dependencies_and_lock(self):
        before = tomllib.loads((self.root / "crates/ninox-core/Cargo.toml").read_text())["package"]["version"]
        major, minor, patch = map(int, before.split("."))
        expected = f"{major}.{minor}.{patch + 1}"
        result = self.run_bump()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), expected)
        manifests = list((self.root / "crates").glob("*/Cargo.toml"))
        for path in manifests:
            data = tomllib.loads(path.read_text())
            self.assertEqual(data["package"]["version"], expected)
            for group in ("dependencies", "dev-dependencies", "build-dependencies"):
                for name, spec in data.get(group, {}).items():
                    if name.startswith("ninox-") and isinstance(spec, dict) and "path" in spec:
                        self.assertEqual(spec["version"], expected, (path, name))
        lock = tomllib.loads((self.root / "Cargo.lock").read_text())
        packages = [p for p in lock["package"] if p["name"].startswith("ninox-")]
        self.assertEqual(len(packages), len(manifests))
        self.assertTrue(all(p["version"] == expected for p in packages))

    def test_rejects_divergent_package_versions_before_writing(self):
        path = self.root / "crates/ninox-core/Cargo.toml"
        original = path.read_text()
        changed = re.sub(r'(?m)^version\s*=\s*"[^"]+"', 'version = "999.0.0"', original, count=1)
        self.assertNotEqual(changed, original)
        path.write_text(changed)
        result = self.run_bump()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("versions differ", result.stderr)
        self.assertEqual(path.read_text(), changed)


if __name__ == "__main__":
    unittest.main()

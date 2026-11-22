import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("release_manifest", Path(__file__).parents[1] / "release-manifest.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class ReleaseManifestTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)

    def packages(self, version="1.2.3"):
        for suffix in ("linux-x86_64.tar.gz", "linux-aarch64.tar.gz", "macos-arm64.dmg", "macos-arm64-app.tar.gz"):
            (self.directory / f"keiki-{version}-{suffix}").write_bytes(b"package")

    def test_manifest_matches_stable_and_beta_artifacts_and_can_be_regenerated(self):
        for version in ("1.2.3", "1.2.3-beta.2"):
            with self.subTest(version=version):
                self.packages(version)
                result = release.build_manifest(version, self.directory)
                self.assertEqual(result["version"], version)
                self.assertEqual(result["minimumMacOS"], "13.0")
                self.assertEqual(len(result["files"]), 4)
                self.assertTrue(all(value["sha256"] == hashlib.sha256(b"package").hexdigest() for value in result["files"].values()))
                (self.directory / "manifest.json").write_text("old manifest")
                self.assertEqual(release.build_manifest(version, self.directory), result)
                for path in self.directory.iterdir():
                    path.unlink()

    def test_empty_or_partial_release_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "missing release artifacts"):
            release.build_manifest("1.2.3", self.directory)
        self.packages()
        (self.directory / "keiki-1.2.3-macos-arm64.dmg").unlink()
        with self.assertRaisesRegex(ValueError, "missing release artifacts"):
            release.build_manifest("1.2.3", self.directory)

    def test_wrong_version_unrelated_and_empty_assets_are_rejected(self):
        self.packages()
        with self.assertRaisesRegex(ValueError, "invalid release version"):
            release.build_manifest("../version", self.directory)
        unexpected = self.directory / "zeron-1.2.3-linux-x86_64.tar.gz"
        unexpected.write_bytes(b"wrong product")
        with self.assertRaisesRegex(ValueError, "unexpected release artifact"):
            release.build_manifest("1.2.3", self.directory)
        unexpected.unlink()
        (self.directory / "keiki-1.2.3-macos-arm64.dmg").write_bytes(b"")
        with self.assertRaisesRegex(ValueError, "empty release artifact"):
            release.build_manifest("1.2.3", self.directory)


if __name__ == "__main__":
    unittest.main()

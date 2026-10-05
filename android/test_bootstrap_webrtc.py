"""Offline archive/bootstrap regressions; coordinator may run after integration."""
import importlib.util
from pathlib import Path
import tempfile
import unittest
import zipfile

spec = importlib.util.spec_from_file_location("bootstrap_webrtc", Path(__file__).with_name("bootstrap-webrtc.py"))
bootstrap = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bootstrap)


class ArchiveTests(unittest.TestCase):
    def archive(self, stage, members):
        path = stage / "archive.zip"
        with zipfile.ZipFile(path, "w") as bundle:
            for name, value in members.items():
                bundle.writestr(name, value)
        return path

    def test_fresh_tree_has_archive_headers_and_java_from_same_release(self):
        with tempfile.TemporaryDirectory() as tmp:
            stage = Path(tmp)
            names = ["libwebrtc.a", "libwebrtc.jar", "webrtc.ninja", "third_party/boringssl/src/include/openssl/ssl.h"]
            archive = self.archive(stage, {f"{bootstrap.TRIPLE}/{n}": b"fixture" for n in names})
            bootstrap.extract_archive(archive, stage)
            bootstrap.validate_tree(stage / bootstrap.TRIPLE)

    def test_traversal_absolute_paths_and_wrong_triple_are_rejected(self):
        for name in ["../escape", "/tmp/escape", f"{bootstrap.TRIPLE}/../../escape", "linux-x64-release/libwebrtc.a"]:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as tmp:
                stage = Path(tmp)
                with self.assertRaises(ValueError):
                    bootstrap.extract_archive(self.archive(stage, {name: b"x"}), stage)

    def test_incomplete_cache_is_not_used(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(ValueError):
                bootstrap.validate_tree(Path(tmp))


if __name__ == "__main__":
    unittest.main()

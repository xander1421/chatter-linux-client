"""Offline archive/bootstrap regressions; coordinator may run after integration."""
import importlib.util
from pathlib import Path
import os
import stat
from unittest.mock import patch
import tempfile
import unittest
import zipfile

spec = importlib.util.spec_from_file_location("bootstrap_webrtc", Path(__file__).with_name("bootstrap-webrtc.py"))
bootstrap = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bootstrap)


# Selected required members from the actual webrtc-51ef663 Android arm64 zip.
REAL_LAYOUT = ["lib/libwebrtc.a", "libwebrtc.jar", "webrtc.ninja",
               "include/api/peer_connection_interface.h",
               "include/third_party/boringssl/src/include/openssl/ssl.h"]


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
            names = REAL_LAYOUT
            archive = self.archive(stage, {f"{bootstrap.TRIPLE}/{n}": b"fixture" for n in names})
            bootstrap.extract_archive(archive, stage)
            bootstrap.validate_tree(stage / bootstrap.TRIPLE)

    def test_traversal_absolute_paths_and_wrong_triple_are_rejected(self):
        for name in ["../escape", "/tmp/escape", f"{bootstrap.TRIPLE}/../../escape", "linux-x64-release/libwebrtc.a"]:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as tmp:
                stage = Path(tmp)
                with self.assertRaises(ValueError):
                    bootstrap.extract_archive(self.archive(stage, {name: b"x"}), stage)

    def test_each_required_real_archive_member_is_checked(self):
        for missing in REAL_LAYOUT:
            with self.subTest(missing=missing), tempfile.TemporaryDirectory() as tmp:
                stage = Path(tmp)
                members = {f"{bootstrap.TRIPLE}/{n}": b"fixture"
                           for n in REAL_LAYOUT if n != missing}
                with self.assertRaisesRegex(ValueError, "missing " + missing):
                    bootstrap.extract_archive(self.archive(stage, members), stage)

    def test_symlinks_and_backslashes_are_rejected(self):
        for name in [f"{bootstrap.TRIPLE}/link", f"{bootstrap.TRIPLE}/bad\\path"]:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as tmp:
                stage = Path(tmp)
                entry = zipfile.ZipInfo(name)
                entry.create_system = 3
                entry.external_attr = ((stat.S_IFREG if "\\" in name else stat.S_IFLNK) | 0o777) << 16
                with zipfile.ZipFile(stage / "archive.zip", "w") as bundle:
                    bundle.writestr(entry, "outside")
                with self.assertRaises(ValueError):
                    bootstrap.extract_archive(stage / "archive.zip", stage)

    def test_expansion_limit_is_enforced(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(bootstrap, "MAX_EXPANDED", 1):
            stage = Path(tmp)
            with self.assertRaisesRegex(ValueError, "expansion limit"):
                bootstrap.extract_archive(self.archive(stage, {f"{bootstrap.TRIPLE}/file": b"xx"}), stage)

    def test_bootstrap_publishes_complete_tree_and_reuses_cache(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {}, clear=True):
            stage = Path(tmp)
            (stage / "Cargo.lock").write_text('[[package]]\nname = "webrtc-sys-build"\nversion = "0.3.18"\n')
            archive = self.archive(stage, {f"{bootstrap.TRIPLE}/{n}": b"fixture" for n in REAL_LAYOUT})
            with patch.object(bootstrap.urllib.request, "urlopen", side_effect=lambda *a, **k: archive.open("rb")) as download:
                root = bootstrap.bootstrap(stage, stage / "target")
                bootstrap.validate_tree(root)
                self.assertEqual(bootstrap.bootstrap(stage, stage / "target"), root)
                download.assert_called_once()
                self.assertEqual(list(root.parent.glob(".bootstrap-*")), [])

    def test_incomplete_download_is_not_published(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {}, clear=True):
            stage = Path(tmp)
            (stage / "Cargo.lock").write_text('[[package]]\nname = "webrtc-sys-build"\nversion = "0.3.18"\n')
            archive = self.archive(stage, {f"{bootstrap.TRIPLE}/webrtc.ninja": b"fixture"})
            with patch.object(bootstrap.urllib.request, "urlopen", side_effect=lambda *a, **k: archive.open("rb")):
                with self.assertRaisesRegex(ValueError, "Incomplete pinned"):
                    bootstrap.bootstrap(stage, stage / "target")
            parent = stage / "target" / "livekit_webrtc" / bootstrap.TAG
            self.assertFalse((parent / bootstrap.TRIPLE).exists())
            self.assertEqual(list(parent.glob(".bootstrap-*")), [])

    def test_pin_guard_precedes_custom_tree(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {"LK_CUSTOM_WEBRTC": tmp}):
            stage = Path(tmp)
            (stage / "Cargo.lock").write_text('[[package]]\nname = "webrtc-sys-build"\nversion = "0.3.19"\n')
            with self.assertRaisesRegex(ValueError, "SDK pin changed"):
                bootstrap.bootstrap(stage, stage / "target")

    def test_incomplete_cache_is_not_used(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(ValueError):
                bootstrap.validate_tree(Path(tmp))


if __name__ == "__main__":
    unittest.main()

import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import zipfile

spec = importlib.util.spec_from_file_location("prepare_swappy", Path(__file__).with_name("prepare-swappy.py"))
sdk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sdk)


class StagingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.output = self.root / "output"
        self.runtime = self.root / "runtime.so"
        self.runtime.write_bytes(b"test NDK runtime")
        self.archive = self.root / "sdk.aar"
        with zipfile.ZipFile(self.archive, "w") as archive:
            archive.writestr(sdk.ENTRY, b"test SDK library")
        self.pin = patch.object(sdk, "SHA256", hashlib.sha256(self.archive.read_bytes()).hexdigest())
        self.pin.start()
        self.addCleanup(self.pin.stop)

    def stage(self):
        sdk.stage(self.archive, self.runtime, self.output)

    def test_owned_runtime_is_removed_on_normal_build(self):
        self.stage()
        self.assertEqual((self.output / "libswappy.so").read_bytes(), b"test SDK library")
        self.assertEqual((self.output / "libc++_shared.so").read_bytes(), self.runtime.read_bytes())
        sdk.disable(self.output)
        self.assertEqual(list(self.output.iterdir()), [])

    def test_preexisting_runtime_is_preserved(self):
        self.output.mkdir()
        runtime = self.output / "libc++_shared.so"
        runtime.write_bytes(b"another dependency")
        self.stage()
        sdk.disable(self.output)
        self.assertEqual(runtime.read_bytes(), b"another dependency")

    def test_replaced_runtime_is_not_deleted(self):
        self.stage()
        runtime = self.output / "libc++_shared.so"
        runtime.write_bytes(b"replacement from another dependency")
        sdk.disable(self.output)
        self.assertTrue(runtime.exists())
        self.assertFalse((self.output / sdk.MARKER).exists())

    def test_bad_checksum_does_not_overwrite_staged_files(self):
        self.stage()
        self.archive.write_bytes(b"untrusted archive")
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            self.stage()
        self.assertEqual((self.output / "libswappy.so").read_bytes(), b"test SDK library")

    def test_release_refusal_precedes_download_or_staging(self):
        with patch.dict("os.environ", {"RELEASE": "1", "CHATTER_ANDROID_SWAPPY_EXPERIMENT": "1"}), \
             patch("sys.argv", ["prepare-swappy.py", "--ndk", str(self.root), "--output-dir", str(self.output)]), \
             patch.object(sdk, "acquire") as acquire:
            with self.assertRaises(SystemExit) as result:
                sdk.main()
            self.assertEqual(result.exception.code, 2)
            acquire.assert_not_called()
        self.assertFalse(self.output.exists())


if __name__ == "__main__":
    unittest.main()

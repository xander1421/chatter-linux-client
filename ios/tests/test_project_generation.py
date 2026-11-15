"""Exercise XcodeGen so native capabilities survive project regeneration."""
import json
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile
import unittest


@unittest.skipUnless(shutil.which("xcodegen"), "requires XcodeGen on the Mac build host")
class ProjectGenerationTests(unittest.TestCase):
    def test_regenerated_app_keeps_call_capabilities(self):
        with tempfile.TemporaryDirectory() as directory:
            ios = Path(directory) / "ios"
            shutil.copytree(
                Path(__file__).resolve().parents[1], ios,
                ignore=shutil.ignore_patterns("build", "__pycache__", "*.xcresult"),
            )
            subprocess.run(["xcodegen", "generate"], cwd=ios, check=True, capture_output=True)
            info = plistlib.loads((ios / "Info.plist").read_bytes())
            self.assertIn("audio", info["UIBackgroundModes"])
            self.assertTrue(info["NSCameraUsageDescription"])
            project = json.loads(subprocess.check_output([
                "plutil", "-convert", "json", "-o", "-",
                str(ios / "Chatter.xcodeproj/project.pbxproj"),
            ]))
            configurations = [obj["buildSettings"] for obj in project["objects"].values()
                              if obj.get("isa") == "XCBuildConfiguration"
                              and obj.get("buildSettings", {}).get("PRODUCT_BUNDLE_IDENTIFIER") == "com.chatter.client"]
            self.assertEqual(len(configurations), 2)
            for settings in configurations:
                self.assertIn("CoreVideo", settings["OTHER_LDFLAGS"])
                self.assertIn("AVFoundation", settings["OTHER_LDFLAGS"])


if __name__ == "__main__":
    unittest.main()

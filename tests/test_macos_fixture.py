"""Offline structural checks only: not an Xcode/YAML parser or macOS build test."""

from pathlib import Path
import plistlib
import re
import subprocess
import unittest
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "tests/fixtures/ios-smoke"
WORKFLOW = ROOT / ".github/workflows/macos-worker.yml"


class MacOSFixtureStructure(unittest.TestCase):
    def test_plist_is_synthetic_application(self):
        with (FIXTURE / "IOSSmoke/Info.plist").open("rb") as source:
            info = plistlib.load(source)
        self.assertEqual(info["CFBundlePackageType"], "APPL")
        self.assertEqual(info["CFBundleIdentifier"], "$(PRODUCT_BUNDLE_IDENTIFIER)")
        self.assertNotIn("UIBackgroundModes", info)
        project = (FIXTURE / "IOSSmoke.xcodeproj/project.pbxproj").read_text()
        self.assertEqual(project.count("org.example.taskmanager.iossmoke"), 2)
        self.assertNotIn("DEVELOPMENT_TEAM", project)
        self.assertNotIn("PBXShellScriptBuildPhase", project)
        self.assertNotIn("XCRemoteSwiftPackageReference", project)

    def test_project_references_resolve(self):
        project = (FIXTURE / "IOSSmoke.xcodeproj/project.pbxproj").read_text()
        # Strip comments so descriptions do not masquerade as project references.
        project = re.sub(r"/\*.*?\*/|//[^\n]*", "", project, flags=re.S)
        definitions = re.findall(r"\b([A-F0-9]{24})\s*=\s*\{\s*isa\s*=", project)
        references = set(re.findall(r"\b[A-F0-9]{24}\b", project))
        self.assertEqual(len(definitions), len(set(definitions)))
        self.assertEqual(references, set(definitions))
        self.assertEqual(len(definitions), 18)
        for name in ("AppDelegate.swift", "Info.plist"):
            self.assertTrue((FIXTURE / "IOSSmoke" / name).is_file())

    def test_shared_scheme_matches_target(self):
        scheme = ET.parse(
            FIXTURE / "IOSSmoke.xcodeproj/xcshareddata/xcschemes/IOSSmoke.xcscheme"
        )
        refs = scheme.findall(".//BuildableReference")
        self.assertTrue(refs)
        for ref in refs:
            self.assertEqual(ref.attrib["BlueprintIdentifier"], "A00000000000000000000009")
            self.assertEqual(ref.attrib["ReferencedContainer"], "container:IOSSmoke.xcodeproj")
            self.assertEqual(ref.attrib["BuildableName"], "IOSSmoke.app")
        project = (FIXTURE / "IOSSmoke.xcodeproj/project.pbxproj").read_text()
        self.assertIn("isa = PBXNativeTarget;", project)

    def test_fixture_build_is_simulator_only_and_bounded(self):
        script = (FIXTURE / "build-simulator.sh").read_text()
        for setting in (
            "-sdk iphonesimulator",
            "generic/platform=iOS Simulator",
            '-derivedDataPath "$fixture/out/DerivedData"',
            "-jobs 2",
            "CODE_SIGNING_ALLOWED=NO",
            "ARCHS=arm64",
            "/usr/bin/ditto -c -k --keepParent",
            "shasum -a 256",
        ):
            self.assertIn(setting, script)
        self.assertIn("/out/", (FIXTURE / ".gitignore").read_text().splitlines())
        for forbidden in ("simctl", "-allowProvisioningUpdates", "-downloadPlatform", "sudo "):
            self.assertNotIn(forbidden, script)
        self.assertIn("git rev-parse HEAD", script)
        self.assertIn("xcodebuild -version", script)

    def test_shell_syntax_without_running_builds(self):
        subprocess.run(["bash", "-n", str(FIXTURE / "build-simulator.sh")], check=True)
        # Validate each literal workflow shell block, without interpreting expressions.
        workflow = WORKFLOW.read_text()
        blocks = re.findall(r"^        run: \|\n((?:          .*\n|\n)+)", workflow, re.M)
        self.assertEqual(len(blocks), 4)
        for block in blocks:
            script = "\n".join(line[10:] for line in block.splitlines())
            subprocess.run(["bash", "-n"], input=script, text=True, check=True)

    def test_workflow_is_manual_gated_and_pinned(self):
        workflow = WORKFLOW.read_text()
        trigger = workflow.split("\non:\n", 1)[1].split("\npermissions:", 1)[0]
        self.assertEqual(re.findall(r"^  ([\w_]+):", trigger, re.M), ["workflow_dispatch"])
        self.assertIn("default: false", trigger)
        self.assertEqual(workflow.count("if: ${{ inputs.acknowledge_unverified_template"), 3)
        self.assertIn("inputs.acknowledge_unverified_template && inputs.build_worker", workflow)
        self.assertIn("SETUP ONLY: no worker, pairing or remote execution", workflow)
        actions = re.findall(r"uses: ([^\s]+)", workflow)
        self.assertEqual(len(actions), 6)
        for action in actions:
            self.assertRegex(action, r"^actions/(checkout|upload-artifact)@[a-f0-9]{40}$")
        self.assertIn("persist-credentials: false", workflow)
        self.assertIn("contents: read", workflow)
        for forbidden in ("secrets.", "contents: write", "pull_request:", "push:", "sudo ", "curl "):
            self.assertNotIn(forbidden, workflow)

    def test_workflow_variants_preserve_first_binary(self):
        workflow = WORKFLOW.read_text()
        first = workflow.index("cargo build --locked --release --target aarch64-apple-darwin -j 2 --no-default-features --bin tasks")
        copy = workflow.index('/release/tasks" "$RUNNER_TEMP/macos-worker/bundle/tasks-headless"')
        second = workflow.index("cargo build --locked --release --target aarch64-apple-darwin -j 2 --bin tasks")
        self.assertLess(first, copy)
        self.assertLess(copy, second)
        self.assertIn('test "$(uname -m)" = arm64', workflow)
        self.assertIn("ditto -c -k --keepParent", workflow)
        self.assertIn("shasum -a 256", workflow)
        self.assertIn("rustc -Vv", workflow)

    def test_fetch_paths_are_normalized_and_outputs_stay_ignored(self):
        doc = (ROOT / "docs/MACOS-FIRST-RUN.md").read_text()
        for name in ("IOSSmoke.app.zip", "SHA256SUMS", "BUILD-INFO.txt"):
            relative = Path("tests/fixtures/ios-smoke/out") / name
            self.assertFalse(relative.is_absolute())
            self.assertNotIn("..", relative.parts)
            self.assertIn(relative.as_posix(), doc)
            ignored = subprocess.run(
                ["git", "check-ignore", "--", relative.as_posix()],
                cwd=ROOT, capture_output=True, text=True, check=True,
            )
            self.assertEqual(ignored.stdout.strip(), relative.as_posix())


if __name__ == "__main__":
    unittest.main()

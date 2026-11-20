# Copyright 2026 Advanced Micro Devices, Inc.
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import hashlib
import io
import json
import os
from pathlib import Path
import select
import shutil
import signal
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parent.parent

HOST_TOOLS = (
    "bwrap",
    "slirp4netns",
    "unshare",
    "nsenter",
    "iptables",
    "ip6tables",
    "iptables-restore",
    "ip6tables-restore",
)
HOST_PACKAGES = ["bubblewrap", "slirp4netns", "util-linux", "iptables"]

linux_only = unittest.skipUnless(
    sys.platform.startswith("linux"), "host packages are offered only on Linux"
)


FAKE_GH = r"""#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

root = Path(os.environ["INSTALLER_FIXTURE_ROOT"])
args = sys.argv[1:]
(root / "gh.log").open("a", encoding="utf-8").write(json.dumps(args) + "\n")
if args[:2] in (["release", "verify-asset"], ["attestation", "verify"]):
    if os.environ.get("FAIL_PROVENANCE") == "1" and args[:2] == ["release", "verify-asset"]:
        raise SystemExit(1)
elif args[:1] == ["api"]:
    print("v1.2.3")
else:
    raise SystemExit(2)
"""


FAKE_CURL = r"""#!/usr/bin/env python3
import os
from pathlib import Path
import shutil
import sys

root = Path(os.environ["INSTALLER_FIXTURE_ROOT"])
args = sys.argv[1:]
destination = Path(args[args.index("--output") + 1])
url = args[-1]
source = root / ("checksum" if url.endswith(".sha256") else "archive")
shutil.copyfile(source, destination)
"""

FAKE_SUDO = r"""#!/bin/sh
exec "$@"
"""

FAKE_APT_GET = r"""#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

root = Path(os.environ["INSTALLER_FIXTURE_ROOT"])
entry = {
    "args": sys.argv[1:],
    "stdin_is_null": os.path.samestat(os.fstat(0), os.stat("/dev/null")),
}
(root / "apt-get.log").open("a", encoding="utf-8").write(json.dumps(entry) + "\n")
"""


class UnixInstallerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name, content in (
            ("gh", FAKE_GH),
            ("curl", FAKE_CURL),
            ("sudo", FAKE_SUDO),
            ("apt-get", FAKE_APT_GET),
        ):
            path = self.bin / name
            path.write_text(content, encoding="utf-8")
            path.chmod(0o755)
        self.tools = self.root / "tools"
        self.tools.mkdir()
        for name in HOST_TOOLS:
            path = self.tools / name
            path.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            path.chmod(0o755)
        archive = self.root / "archive"
        with tarfile.open(archive, "w:gz", format=tarfile.PAX_FORMAT) as output:
            for name in ("axis", "axisd", "axis-seccomp-launcher", "lxc-exec"):
                info = tarfile.TarInfo(f"axis-linux-x86_64/{name}")
                info.mode = 0o755
                data = name.encode("ascii")
                info.size = len(data)
                output.addfile(info, io.BytesIO(data))
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        (self.root / "checksum").write_text(
            f"{digest}  axis-linux-x86_64.tar.gz\n", encoding="ascii"
        )
        self.environment = os.environ | {
            "PATH": f"{self.bin}:{os.environ['PATH']}",
            "HOME": str(self.root / "home"),
            "INSTALLER_FIXTURE_ROOT": str(self.root),
            "AXIS_INSTALL_HOST_TOOL_DIRS": str(self.tools),
        }

    def tearDown(self):
        self.temporary.cleanup()

    def run_installer_on_terminal(self, answer: str) -> str:
        import pty

        pid, terminal = pty.fork()
        if pid == 0:
            try:
                os.execve(
                    "/bin/sh",
                    ["sh", str(ROOT / "install.sh"), "--prefix", str(self.root / "installed")],
                    self.environment,
                )
            finally:
                os._exit(127)
        output = b""
        answered = False
        deadline = time.monotonic() + 20
        try:
            while time.monotonic() < deadline:
                ready, _, _ = select.select([terminal], [], [], 0.1)
                if not ready:
                    continue
                try:
                    chunk = os.read(terminal, 4096)
                except OSError:
                    chunk = b""
                if not chunk:
                    break
                output += chunk
                if not answered and b"[Y/n]" in output:
                    os.write(terminal, answer.encode("ascii") + b"\n")
                    answered = True
            else:
                os.kill(pid, signal.SIGKILL)
        finally:
            os.close(terminal)
            _, status = os.waitpid(pid, 0)
        text = output.decode(errors="replace")
        self.assertEqual(os.waitstatus_to_exitcode(status), 0, text)
        return text

    def apt_get_log(self) -> list[dict]:
        path = self.root / "apt-get.log"
        if not path.exists():
            return []
        return [json.loads(line) for line in path.read_text().splitlines()]

    def run_installer(self, **environment: str) -> subprocess.CompletedProcess[str]:
        install = self.root / "installed"
        return subprocess.run(
            ["sh", str(ROOT / "install.sh"), "--prefix", str(install)],
            env=self.environment | environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=20,
        )

    def test_remote_install_requires_release_and_workflow_provenance(self):
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stdout)
        calls = [
            json.loads(line) for line in (self.root / "gh.log").read_text().splitlines()
        ]
        self.assertTrue(any(call[:2] == ["release", "verify-asset"] for call in calls))
        attestation = next(
            call
            for call in calls
            if call[:2] == ["attestation", "verify"] and "--help" not in call
        )
        self.assertIn("ROCm/axis/.github/workflows/release.yml", attestation)
        self.assertIn("--source-ref=refs/tags/v1.2.3", attestation)
        self.assertIn("--deny-self-hosted-runners", attestation)

    def test_provenance_failure_prevents_installation(self):
        result = self.run_installer(FAIL_PROVENANCE="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / "installed/axis").exists())

    def test_trusted_local_archive_skip_returns_success(self):
        local_archive = self.root / "axis-linux-x86_64.tar.gz"
        local_archive.write_bytes((self.root / "archive").read_bytes())
        local_checksum = self.root / "axis-linux-x86_64.tar.gz.sha256"
        local_checksum.write_bytes((self.root / "checksum").read_bytes())
        result = self.run_installer(
            AXIS_INSTALL_ARCHIVE=str(local_archive),
            AXIS_INSTALL_SHA256=str(local_checksum),
        )
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertTrue((self.root / "installed/axis").is_file())

    def test_oversized_checksum_download_fails_closed(self):
        (self.root / "checksum").write_bytes(b"x" * (1024 * 1024 + 1))
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exceeds size limit", result.stdout)

    @linux_only
    def test_present_host_tools_offer_no_packages(self):
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertNotIn("Missing host tools", result.stdout)
        self.assertEqual(self.apt_get_log(), [])

    @linux_only
    def test_missing_host_tools_without_terminal_print_the_command(self):
        (self.tools / "slirp4netns").unlink()
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertTrue((self.root / "installed/axis").is_file())
        self.assertIn("Missing host tools: slirp4netns\n", result.stdout)
        self.assertIn(
            f"To install them, run: {'' if os.geteuid() == 0 else 'sudo '}apt-get update",
            result.stdout,
        )
        self.assertEqual(self.apt_get_log(), [])

    @linux_only
    def test_terminal_prompt_installs_host_packages_by_default(self):
        (self.tools / "slirp4netns").unlink()
        self.run_installer_on_terminal("")
        log = self.apt_get_log()
        self.assertEqual(
            [entry["args"] for entry in log],
            [["update"], ["install", "-y", *HOST_PACKAGES]],
        )
        self.assertTrue(all(entry["stdin_is_null"] for entry in log))

    @linux_only
    def test_terminal_prompt_declined_installs_nothing(self):
        (self.tools / "slirp4netns").unlink()
        output = self.run_installer_on_terminal("n")
        self.assertIn("To install them, run:", output)
        self.assertEqual(self.apt_get_log(), [])

    def test_windows_installer_contains_equivalent_enforcement(self):
        source = (ROOT / "install.ps1").read_text(encoding="utf-8")
        self.assertIn("$PSVersionTable.PSVersion.Major -lt 7", source)
        self.assertIn("requires PowerShell 7 or later (pwsh)", source)
        self.assertNotIn('powershell -c "irm', source)
        self.assertIn("Receive-BoundedFile", source)
        self.assertIn("release verify-asset", source)
        self.assertIn("attestation verify", source)
        self.assertIn("--signer-workflow", source)
        self.assertIn("--deny-self-hosted-runners", source)
        self.assertNotIn("ReadToEndAsync", source)
        self.assertIn("ReadBounded", source)
        self.assertIn("JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE", source)
        self.assertIn("CREATE_SUSPENDED", source)
        self.assertIn("AssignProcessToJobObject", source)
        self.assertIn("TerminateJobObject", source)
        self.assertIn("Task.WhenAll(stdoutTask, stderrTask)", source)
        self.assertEqual(source.count("FileAccess.Read, 65536, false"), 2)
        self.assertNotIn("FileAccess.Read, 65536, true", source)
        self.assertNotIn("$Process.Kill", source)
        self.assertNotIn("taskkill.exe", source)

        readme = (ROOT / "README.md").read_text(encoding="utf-8")
        setup = (ROOT / "docs/setup-and-install.md").read_text(encoding="utf-8")
        self.assertIn("PowerShell 7 or later", readme)
        self.assertIn("PowerShell 7 or later", setup)
        self.assertIn("https://aka.ms/powershell-release", setup)


@unittest.skipUnless(
    shutil.which("pwsh") and sys.platform == "win32",
    "PowerShell 7 on Windows is unavailable",
)
class PowerShellInstallerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        source = (ROOT / "install.ps1").read_text(encoding="utf-8")
        source, invocation = source.rsplit("\nInstall-AXIS", 1)
        self.assertEqual(invocation.strip(), "")
        self.fake_gh = self.root / "fake_gh.py"
        self.fake_gh.write_text(
            """import os
from pathlib import Path
import subprocess
import sys
import time

mode = sys.argv[1]
if mode in ('tree', 'orphan'):
    marker = sys.argv[2]
    subprocess.Popen([
        sys.executable,
        '-c',
        \"import time; from pathlib import Path; time.sleep(3); \"
        f\"Path({marker!r}).write_text('survived'); time.sleep(30)\",
    ], close_fds=False)
    if mode == 'orphan':
        raise SystemExit(0)
    mode = 'stdout'
descriptor = 1 if mode == 'stdout' else 2
while True:
    os.write(descriptor, b'x' * 65536)
""",
            encoding="utf-8",
        )
        harness = (
            source
            + r"""
$script:GhPath = $env:PYTHON_EXE
$script:GhCommandTimeoutMilliseconds = 2000
try {
    if ($env:GH_TEST_MODE -in @("tree", "orphan")) {
        Invoke-Gh $env:FAKE_GH $env:GH_TEST_MODE $env:SURVIVAL_MARKER | Out-Null
    } else {
        Invoke-Gh $env:FAKE_GH $env:GH_TEST_MODE | Out-Null
    }
    exit 90
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 42
}
"""
        )
        self.harness = self.root / "invoke-gh-test.ps1"
        self.harness.write_text(harness, encoding="utf-8")

    def tearDown(self):
        self.temporary.cleanup()

    def run_probe(self, mode: str) -> subprocess.CompletedProcess[str]:
        marker = self.root / "descendant-survived"
        environment = os.environ | {
            "PYTHON_EXE": sys.executable,
            "FAKE_GH": str(self.fake_gh),
            "GH_TEST_MODE": mode,
            "SURVIVAL_MARKER": str(marker),
        }
        return subprocess.run(
            ["pwsh", "-NoLogo", "-NoProfile", "-File", str(self.harness)],
            env=environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=15,
        )

    def test_caps_stdout_and_stderr_while_process_is_running(self):
        for mode in ("stdout", "stderr"):
            with self.subTest(mode=mode):
                result = self.run_probe(mode)
                self.assertEqual(result.returncode, 42, result.stderr)
                self.assertIn("output exceeds size limit", result.stderr)

    def test_output_cap_kills_the_process_tree_immediately(self):
        marker = self.root / "descendant-survived"
        result = self.run_probe("tree")
        self.assertEqual(result.returncode, 42, result.stderr)
        self.assertIn("output exceeds size limit", result.stderr)
        time.sleep(4)
        self.assertFalse(marker.exists(), "gh descendant survived output rejection")

    def test_timeout_kills_descendant_after_root_exits_with_inherited_handles(self):
        marker = self.root / "descendant-survived"
        result = self.run_probe("orphan")
        self.assertEqual(result.returncode, 42, result.stderr)
        self.assertIn("timed out", result.stderr)
        time.sleep(4)
        self.assertFalse(
            marker.exists(), "gh descendant survived root exit and timeout"
        )


if __name__ == "__main__":
    unittest.main()

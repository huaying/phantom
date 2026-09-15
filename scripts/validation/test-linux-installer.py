#!/usr/bin/env python3
"""Exercise the real installer functions with isolated files and process stubs."""
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest

INSTALLER = (Path(os.environ["PHANTOM_TEST_INSTALLER"])
             if "PHANTOM_TEST_INSTALLER" in os.environ
             else Path(__file__).resolve().parents[2] / "install.sh")


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="phantom-installer-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.functions = self.root / "functions.sh"
        source = INSTALLER.read_text()
        self.assertTrue(source.rstrip().endswith('main "$@"'))
        self.functions.write_text(source.rsplit('main "$@"', 1)[0])
        self.bin = self.root / "tools"
        self.bin.mkdir()
        self.executable(self.bin / "sudo", '#!/bin/sh\nif [ "$1" = -u ]; then shift 2; fi\nexec "$@"\n')
        for name in ("pkill", "pgrep"):
            self.executable(self.bin / name, "#!/bin/sh\nexit 1\n")
        self.install = self.root / 'candidate space\' "dollar$ `tick` %f \\path'
        self.install.mkdir()
        self.server = self.install / "phantom-server"
        self.executable(self.server, '#!/bin/sh\nprintf "%s\\n" "$0" "$@" > "$PHANTOM_TEST_RESULT"\n')
        self.executable(self.bin / "phantom-server", "#!/bin/sh\nexit 97\n")
        self.user_dir = self.root / "user"
        self.user_dir.mkdir()
        self.result = self.root / "launched.txt"
        self.env = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ["PATH"],
                        PHANTOM_INSTALL_DIR=str(self.install), PHANTOM_TEST_RESULT=str(self.result),
                        USER_HOME=str(self.user_dir), TARGET_USER="fixture")

    @staticmethod
    def executable(path, source):
        path.write_text(source)
        path.chmod(0o755)

    def shell(self, code, **env):
        return subprocess.run(["sh", "-c", '. "$1"\n' + code, "sh", str(self.functions)],
                              cwd=self.root, env=dict(self.env, **env), text=True, capture_output=True)

    def test_selected_install_wins_over_old_path_binary(self):
        result = self.shell("linux_phantom_server_bin")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), str(self.server))

    def test_local_asset_creates_relative_destination(self):
        result = self.shell('download_and_install phantom-server\nprintf "RESOLVED=%s\\n" "$INSTALL_DIR"',
                            PHANTOM_INSTALL_DIR="new relative directory", PHANTOM_SERVER_BIN=str(self.server))
        self.assertEqual(result.returncode, 0, result.stderr)
        target = self.root / "new relative directory" / "phantom-server"
        self.assertEqual(target.read_bytes(), self.server.read_bytes())
        self.assertTrue(os.access(target, os.X_OK))
        self.assertIn("RESOLVED=" + str(target.parent.resolve()), result.stdout)

    def test_desktop_launch_preserves_literal_path_and_arguments(self):
        try:
            from gi.repository import Gio
        except ImportError:
            if os.environ.get("PHANTOM_TEST_REQUIRE_GIO") == "1":
                self.fail("Gio is required for desktop-launch coverage")
            self.skipTest("Gio desktop parser is only available in the Linux test environment")
        result = self.shell("linux_install_autostart")
        self.assertEqual(result.returncode, 0, result.stderr)
        desktop = self.user_dir / ".config/autostart/phantom-server.desktop"
        app = Gio.DesktopAppInfo.new_from_filename(str(desktop))
        self.assertIsNotNone(app, desktop.read_text())
        context = Gio.AppLaunchContext()
        for key, value in self.env.items():
            context.setenv(key, value)
        self.assertTrue(app.launch([], context))
        until = time.monotonic() + 5
        while not self.result.exists() and time.monotonic() < until:
            time.sleep(0.02)
        self.assertTrue(self.result.exists(), desktop.read_text())
        self.assertEqual(self.result.read_text().splitlines(),
                         [str(self.server), "--no-encrypt", "--transport", "tcp,web"])

    def test_line_break_does_not_create_desktop_entry(self):
        result = self.shell("linux_install_autostart", PHANTOM_INSTALL_DIR="invalid\nExec=unexpected")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.user_dir / ".config/autostart/phantom-server.desktop").exists())


if __name__ == "__main__":
    unittest.main()

import json
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))

from embedded_ai_tool import cli, remote
from embedded_ai_tool.cli import parser, run
from embedded_ai_tool.core import Profile

FAKE = textwrap.dedent('''
    import json, signal, sys, time
    args = sys.argv[1:]
    if "connect" in args:
        if "--die" in args:
            print(json.dumps({"event": "closed", "reason": "Servis bulunamadı"}), flush=True)
            sys.exit(4)
        print(json.dumps({"event": "ready", "device": "lab-pi", "services": []}), flush=True)
        signal.signal(signal.SIGTERM, lambda *a: sys.exit(0))
        while True:
            time.sleep(0.1)
    if "status" in args:
        print(json.dumps({"ok": True, "connected": True}))
    elif "put" in args:
        print(json.dumps({"ok": True, "path": "/var/lib/x/f.elf", "sha256": "0" * 64}))
    else:
        print(json.dumps({"ok": False, "error": "kötü"}))
        sys.exit(1)
''')


class RemoteTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        script = self.root / "sahte.py"
        script.write_text(FAKE, encoding="utf-8")
        self.prefix = [sys.executable, str(script)]
        self.data = {"schema_version": 1, "name": "fixture", "byte_order": "little",
                     "connection": {"host": "127.0.0.1", "port": 6666},
                     "identity": {"address": "0x1000", "size": 4, "expected_hex": "01020304"},
                     "read_ranges": [{"address": "0x1000", "size": "0x2000"}],
                     "flash_ranges": [], "openocd": {"remote_files": True}, "remote": {"session": "lab"}}
        self.path = self.root / "board.json"

    def profile(self):
        self.path.write_text(json.dumps(self.data), encoding="utf-8")
        return Profile(self.path)

    def test_connect_status_disconnect(self):
        key = self.root / "k"
        ready = remote.connect(self.prefix, "lab", key, [], 10, self.root / "log")
        self.assertEqual(ready["device"], "lab-pi")
        self.assertTrue(remote.status(self.prefix, "lab")["connected"])
        import os
        import signal
        os.kill(ready["pid"], signal.SIGTERM)

    def test_closed_reason_reaches_error(self):
        self.prefix.append("--die")
        with self.assertRaisesRegex(ValueError, "Servis bulunamadı"):
            remote.connect(self.prefix, "lab", self.root / "k", [], 10, self.root / "log")

    def test_error_field_is_reported(self):
        with self.assertRaisesRegex(ValueError, "kötü"):
            remote.ls(self.prefix, "lab")

    def test_mutations_need_execute(self):
        with patch("subprocess.run") as sub, patch("subprocess.Popen") as popen:
            for argv in (["put", "f.bin"], ["rm", "f.bin"], ["openocd", "start"], ["openocd", "stop"],
                         ["exec", "--", "make"]):
                result = run(parser().parse_args(["remote", *argv]))
                self.assertFalse(result["executed"])
            sub.assert_not_called()
            popen.assert_not_called()

    def flash_args(self, *extra):
        sessions = self.root / "sessions"
        sessions.mkdir(exist_ok=True)
        return parser().parse_args(["--profile", str(self.path), "--session-dir", str(sessions), "flash", *extra])

    def fake_image(self, sha):
        image = Mock(sha=sha, path=Path("fw.axf"), raw=b"firmware")
        image.regions.return_value = []
        return image

    def test_remote_flash_rejects_wrong_sha_before_board_is_touched(self):
        profile = self.profile()
        profile.image = self.fake_image("a" * 64)
        with patch.object(cli, "Profile", return_value=profile), patch.object(cli, "open_target") as target, \
                patch.object(remote, "binary", return_value=self.prefix), \
                patch.object(remote, "put", return_value={"sha256": "b" * 64, "path": "/x/f.axf"}):
            with self.assertRaises(ValueError):
                run(self.flash_args("--execute"))
        target.assert_not_called()

    def test_remote_flash_script_uses_remote_path(self):
        profile = self.profile()
        profile.image = self.fake_image("a" * 64)
        script = Mock(return_value="ok")
        with patch.object(cli, "Profile", return_value=profile), patch.object(cli, "open_target"), \
                patch.object(cli, "openocd_only", return_value=Mock(script=script)), \
                patch.object(remote, "binary", return_value=self.prefix), \
                patch.object(remote, "put", return_value={"sha256": "a" * 64, "path": "/var/$x/f.axf"}) as put:
            result = run(self.flash_args("--execute"))
        self.assertIn('"/var/\\$x/f.axf"', script.call_args.args[0])
        self.assertEqual(result["plan"]["remote_name"], "eat-" + "a" * 16 + ".axf")
        # Yüklenen dosya, doğrulanmış baytların dondurulmuş kopyasıdır
        self.assertEqual(Path(put.call_args.args[2]).read_bytes(), b"firmware")

    def test_remote_flash_plan_does_not_upload(self):
        profile = self.profile()
        profile.image = self.fake_image("a" * 64)
        with patch.object(cli, "Profile", return_value=profile), patch.object(remote, "put") as put:
            result = run(self.flash_args())
        put.assert_not_called()
        self.assertFalse(result["executed"])

    def test_profile_rejects_both_filesystem_modes(self):
        self.data["openocd"]["shared_filesystem"] = True
        with self.assertRaises(ValueError):
            self.profile()

    def test_profile_requires_session(self):
        del self.data["remote"]
        with self.assertRaises(ValueError):
            self.profile()
        self.data["remote"] = {"session": "Bad Name"}
        with self.assertRaises(ValueError):
            self.profile()

    def test_key_text_not_in_session_records(self):
        key = self.root / "lab.key"
        key.write_text("NL1-GIZLI", encoding="utf-8")
        sessions = self.root / "sessions"
        with patch.object(cli, "run", return_value={"ok": True}), \
                patch("sys.argv", ["eat", "--session-dir", str(sessions), "remote", "connect",
                                   "--key-file", str(key)]):
            cli.main()
        records = list(sessions.glob("*.json"))
        self.assertTrue(records)
        self.assertFalse(any("NL1-" in f.read_text(encoding="utf-8") for f in records))


if __name__ == "__main__":
    unittest.main()

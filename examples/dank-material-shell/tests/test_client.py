import json, os, socket, subprocess, tempfile, threading, unittest
from pathlib import Path

CLIENT = Path(__file__).resolve().parents[1] / "dashbeam-dms-client"


class ClientTest(unittest.TestCase):
    def test_request_preserves_special_characters(self):
        with tempfile.TemporaryDirectory() as tmp:
            folder = Path(tmp) / "dashbeam"
            folder.mkdir()
            server = socket.socket(socket.AF_UNIX)
            server.bind(str(folder / "control.sock"))
            server.listen(1)
            got = []

            def serve():
                conn, _ = server.accept()
                with conn:
                    data = conn.makefile("rb").readline()
                    got.append(json.loads(data))
                    conn.sendall(b'{"ok":true,"data":{"ticket":"example"}}\n')
                server.close()

            thread = threading.Thread(target=serve)
            thread.start()
            request = {
                "action": "send",
                "paths": ["/tmp/a b;$(touch SHOULD_NOT_EXIST)\nfile"],
                "endpoint_id": "abc",
            }
            result = subprocess.run(
                [str(CLIENT), json.dumps(request)],
                env={**os.environ, "XDG_RUNTIME_DIR": tmp},
                capture_output=True,
                text=True,
                timeout=5,
            )
            thread.join(timeout=2)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(got, [request])
            self.assertTrue(json.loads(result.stdout)["ok"])

    def test_absent_backend_reports_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            r = subprocess.run(
                [str(CLIENT)],
                env={**os.environ, "XDG_RUNTIME_DIR": tmp},
                capture_output=True,
                text=True,
            )
            self.assertEqual(r.returncode, 1)
            self.assertFalse(json.loads(r.stdout)["ok"])


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/python3
"""Exercise two isolated DashBeam instances with a synthetic file only."""

import argparse, hashlib, json, os, socket, subprocess, tempfile, time, shutil
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=Path, required=True)
binary = parser.parse_args().binary.resolve()
folder = Path(tempfile.mkdtemp(prefix="dms-qa-"))
os.chmod(folder, 0o700)
processes = []
logs = []
wayland = os.environ.get("WAYLAND_DISPLAY", "wayland-1")
if not wayland.startswith("/"):
    wayland = str(Path(os.environ["XDG_RUNTIME_DIR"]) / wayland)


def request(runtime, body):
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(120)
        s.connect(str(runtime / "dashbeam/control.sock"))
        s.sendall(json.dumps(body).encode() + b"\n")
        result = json.loads(s.makefile("rb").readline())
        if not result["ok"]:
            raise RuntimeError(result["error"])
        return result["data"]


try:
    runtimes = []
    for name in ["sender", "receiver"]:
        root = folder / name
        root.mkdir()
        runtime = root / "run"
        runtime.mkdir(mode=0o700)
        runtimes.append(runtime)
        env = {
            **os.environ,
            "XDG_DATA_HOME": str(root / "data"),
            "XDG_CONFIG_HOME": str(root / "config"),
            "XDG_CACHE_HOME": str(root / "cache"),
            "XDG_RUNTIME_DIR": str(runtime),
            "WAYLAND_DISPLAY": wayland,
            "ALT_SENDME_ALLOW_MULTI_INSTANCE": "1",
            "DASHBEAM_LOCAL_CONTROL": "1",
            "GDK_BACKEND": "wayland",
        }
        log = open(root / "process.log", "w")
        logs.append(log)
        processes.append(
            subprocess.Popen(
                [str(binary), "--hidden"],
                env=env,
                stdout=log,
                stderr=subprocess.STDOUT,
                cwd=root,
            )
        )
    for runtime, proc in zip(runtimes, processes):
        deadline = time.monotonic() + 90
        while not (runtime / "dashbeam/control.sock").exists():
            if proc.poll() is not None:
                raise RuntimeError("DashBeam terminated; logs: " + str(folder))
            if time.monotonic() > deadline:
                raise TimeoutError("Bridge startup; logs: " + str(folder))
            time.sleep(0.5)
        status = request(runtime, {"action": "status"})
        assert isinstance(status["paired"], list) and isinstance(status["nearby"], list)
    payload = folder / "sample with spaces.txt"
    payload.write_text("DashBeam DMS — synthetic local integration test\n" * 1000)
    share = request(runtimes[0], {"action": "send", "paths": [str(payload)]})
    assert share["ticket"]
    output = folder / "received"
    output.mkdir()
    request(
        runtimes[1],
        {"action": "receive", "ticket": share["ticket"], "output": str(output)},
    )
    deadline = time.monotonic() + 100
    while time.monotonic() < deadline:
        snapshot = request(runtimes[1], {"action": "status"})
        events = snapshot["events"]
        if events.get("receive-error"):
            raise RuntimeError(str(events["receive-error"]))
        received = [p for p in output.rglob("*") if p.is_file()]
        if received and any(p.read_bytes() == payload.read_bytes() for p in received):
            break
        time.sleep(1)
    else:
        raise TimeoutError("Transfer did not complete; logs: " + str(folder))
    request(runtimes[0], {"action": "stop"})
    pairing = request(runtimes[1], {"action": "pairing"})
    request(runtimes[0], {"action": "join", "ticket": pairing})
    deadline = time.monotonic() + 25
    while time.monotonic() < deadline:
        paired = request(runtimes[0], {"action": "status"})["paired"]
        available = [d for d in paired if d["online"]]
        if available:
            break
        time.sleep(1)
    else:
        raise TimeoutError("Paired device not online; logs: " + str(folder))
    target = available[0]["endpoint_id"]
    share = request(
        runtimes[0], {"action": "send", "paths": [str(payload)], "endpoint_id": target}
    )
    assert share["delivered"], "Direct invite not delivered"
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        invite = request(runtimes[1], {"action": "status"})["events"].get(
            "paired-invite-received"
        )
        if invite:
            break
        time.sleep(0.5)
    else:
        raise TimeoutError("Invite not received")
    output2 = folder / "received-from-device"
    output2.mkdir()
    request(
        runtimes[1],
        {
            "action": "respond",
            "endpoint_id": invite["remote_endpoint_id"],
            "accepted": True,
            "output": str(output2),
        },
    )
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        received = [p for p in output2.rglob("*") if p.is_file()]
        if any(p.read_bytes() == payload.read_bytes() for p in received):
            break
        time.sleep(0.5)
    else:
        raise TimeoutError("Direct device transfer did not finish")
    request(runtimes[0], {"action": "stop"})
    report = {
        "ok": True,
        "isolated_instances": 2,
        "bytes": payload.stat().st_size,
        "sha256": hashlib.sha256(payload.read_bytes()).hexdigest(),
        "scratch": str(folder),
        "checks": [
            "device lists",
            "send ticket",
            "receive",
            "file bytes match",
            "stop sharing",
            "pair devices",
            "online presence",
            "send to selected device",
            "accept incoming invite",
            "direct file bytes match",
        ],
    }
    print(json.dumps(report))
finally:
    for p in processes:
        if p.poll() is None:
            p.terminate()
    for p in processes:
        try:
            p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            p.kill()
            p.wait()
    for log in logs:
        log.close()
    if "report" in locals():
        shutil.rmtree(folder)

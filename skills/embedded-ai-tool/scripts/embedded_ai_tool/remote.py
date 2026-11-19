"""openocd-link sarmalayıcısı: uzak kart oturumu, dosya aktarımı ve uzak komutlar."""
import json
import os
import re
import shutil
import subprocess
import time
from pathlib import Path

SESSION_RULE = re.compile(r"^[a-z0-9_-]{1,32}$")


def check_session(session):
    if not isinstance(session, str) or not SESSION_RULE.fullmatch(session):
        raise ValueError("Oturum adı [a-z0-9_-] ve 1..32 karakter olmalı")
    return session


def binary(explicit=None):
    """Komut önekini döner: --binary, EAT_OPENOCD_LINK, sonra PATH."""
    if explicit:
        return list(explicit) if isinstance(explicit, (list, tuple)) else [str(explicit)]
    env = os.environ.get("EAT_OPENOCD_LINK")
    if env:
        return [env]
    found = shutil.which("openocd-link")
    if found:
        return [found]
    raise ValueError("openocd-link bulunamadı")


def last_json(text):
    for line in reversed(text.splitlines()):
        line = line.strip()
        if line.startswith("{"):
            try:
                return json.loads(line)
            except ValueError:
                continue
    return None


def call(prefix, session, args, timeout=600):
    check_session(session)
    done = subprocess.run([*prefix, "--session", session, "--json", *args], stdin=subprocess.DEVNULL,
                          capture_output=True, text=True, errors="replace", timeout=timeout, check=False)
    result = last_json(done.stdout)
    if done.returncode:
        message = result.get("error") if isinstance(result, dict) else None
        raise ValueError(message or done.stderr.strip() or f"openocd-link çıkış kodu {done.returncode}")
    if result is None:
        raise ValueError("openocd-link JSON yanıtı vermedi")
    return result


def scan(path):
    """Log dosyasındaki olay satırlarını okur."""
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return []
    events = []
    for line in text.splitlines():
        if line.startswith("{"):
            try:
                value = json.loads(line)
            except ValueError:
                continue
            if isinstance(value, dict) and "event" in value:
                events.append(value)
    return events


def connect(prefix, session, key_file, maps, timeout, log_dir):
    """Oturum daemon'unu arka planda başlatır ve `ready` olayını bekler."""
    check_session(session)
    if not 1 <= timeout <= 600:
        raise ValueError("timeout 1..600 olmalı")
    log_dir = Path(log_dir)
    log_dir.mkdir(parents=True, exist_ok=True)
    log = log_dir / f"{session}.log"
    argv = [*prefix, "--session", session, "--json", "connect", "--key-file", str(key_file),
            "--timeout", str(timeout)]
    for item in maps:
        argv += ["--map", item]
    options = {}
    if os.name == "nt":
        options["creationflags"] = (getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
                                    | getattr(subprocess, "DETACHED_PROCESS", 0))
    else:
        options["start_new_session"] = True
    with log.open("wb") as stream:
        process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=stream, stderr=stream, **options)
    deadline = time.monotonic() + timeout + 5
    while True:
        events = scan(log)
        for event in events:
            if event["event"] == "ready":
                return {**event, "pid": process.pid, "log": str(log)}
        if process.poll() is not None:
            events = scan(log)
            closed = [e for e in events if e["event"] == "closed"]
            reason = closed[-1].get("reason") if closed else None
            raise ValueError(reason or f"openocd-link ready vermeden bitti (kod {process.returncode})")
        if time.monotonic() >= deadline:
            process.terminate()
            try:
                process.wait(5)
            except subprocess.TimeoutExpired:
                process.kill()
            raise ValueError("Bağlantı zaman aşımı: ready olayı gelmedi")
        time.sleep(0.1)


def status(prefix, session):
    return call(prefix, session, ["status"])


def disconnect(prefix, session):
    return call(prefix, session, ["disconnect"])


def put(prefix, session, path, name=None):
    args = ["put", str(path)]
    if name:
        args += ["--name", name]
    return call(prefix, session, args)


def get(prefix, session, name, out, force=False):
    args = ["get", name, "--out", str(out)]
    if force:
        args.append("--force")
    return call(prefix, session, args)


def ls(prefix, session):
    return call(prefix, session, ["ls"])


def rm(prefix, session, name):
    return call(prefix, session, ["rm", name])


def openocd(prefix, session, action):
    if action not in ("start", "stop", "status", "log"):
        raise ValueError("Geçersiz openocd eylemi")
    return call(prefix, session, ["openocd", action])


def exec(prefix, session, argv, cwd=None, timeout=60):  # noqa: A001 -- plandaki işlev adı.
    args = ["exec", "--timeout", str(timeout)]
    if cwd:
        args += ["--cwd", cwd]
    return call(prefix, session, [*args, "--", *argv], timeout=timeout + 30)

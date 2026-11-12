"""Configure nested rootless Podman in the outer container's writable layer."""
from pathlib import Path
import os
import pwd
import shutil
import stat
import subprocess
import sys


def mapped_ranges(mapping, own_id):
    result = []
    for line in mapping.splitlines():
        start, _, count = map(int, line.split())
        end = start + count
        cursor = start
        # A helper with CAP_SETUID alone cannot delegate outer UID zero:
        # mapping that ID would additionally require CAP_SETFCAP.
        for excluded in sorted({0, own_id}):
            if cursor <= excluded < end:
                if cursor < excluded:
                    result.append((cursor, excluded - cursor))
                cursor = excluded + 1
        if cursor < end:
            result.append((cursor, end - cursor))
    if not result:
        raise RuntimeError("outer container has no subordinate IDs available")
    return result


def main():
    if os.geteuid() != 0:
        raise RuntimeError("setup must run as container root")
    uid, gid = map(int, sys.argv[1:3])
    proc_mount = sys.argv[3] if len(sys.argv) > 3 else "private"
    if proc_mount not in {"private", "host"}:
        raise RuntimeError("proc_mount must be private or host")
    if uid <= 0 or gid < 0:
        raise RuntimeError("setup requires a non-root workspace UID and valid GID")
    tools = {}
    for name in ("podman", "newuidmap", "newgidmap", "setcap"):
        tools[name] = shutil.which(name)
        if not tools[name]:
            raise RuntimeError(f"image is missing {name}; install Podman, UID/GID mapping helpers and libcap tools")
    username = pwd.getpwuid(uid).pw_name
    ranges = {
        "subuid": mapped_ranges(Path("/proc/self/uid_map").read_text(), uid),
        "subgid": mapped_ranges(Path("/proc/self/gid_map").read_text(), gid),
    }
    for helper, capability in (("newuidmap", "cap_setuid=ep"), ("newgidmap", "cap_setgid=ep")):
        path = Path(tools[helper]).resolve(strict=True)
        info = path.stat()
        if not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022:
            raise RuntimeError(f"{helper} must be a root-owned, non-writable regular executable")
        path.chmod(stat.S_IMODE(info.st_mode) & ~(stat.S_ISUID | stat.S_ISGID))
        subprocess.run([tools["setcap"], capability, str(path)], check=True)
    for filename, allocated in ranges.items():
        path = Path("/etc") / filename
        existing = path.read_text().splitlines() if path.exists() else []
        existing = [line for line in existing if line.split(":", 1)[0] not in {username, str(uid)}]
        path.write_text("\n".join(existing + [f"{username}:{start}:{count}" for start, count in allocated]) + "\n")
    runtime = Path(f"/run/user/{uid}")
    runtime.mkdir(parents=True, exist_ok=True)
    os.chown(runtime, uid, gid)
    runtime.chmod(0o700)
    config = Path("/etc/containers/containers.conf.d/90-enclo-nested.conf")
    config.parent.mkdir(parents=True, exist_ok=True)
    volumes = 'volumes = ["/proc:/proc:ro"]\n' if proc_mount == "host" else ""
    config.write_text('[containers]\ncgroups = "disabled"\nnetns = "host"\n' + volumes + '\n[engine]\ncgroup_manager = "cgroupfs"\nremote = false\n')
    print(f"Configured nested rootless Podman for {username} (UID {uid}, GID {gid})")


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, subprocess.CalledProcessError, KeyError) as error:
        raise SystemExit(f"nested_podman setup failed: {error}")

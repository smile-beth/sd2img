"""Validate a main-branch version tag and apply its version for the build."""
import os
from pathlib import Path
import re
import subprocess
import tomllib


VERSION = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")


def parse_version(value):
    match = VERSION.fullmatch(value)
    if not match:
        raise ValueError(f"Expected a stable MAJOR.MINOR.PATCH version: {value!r}")
    return tuple(map(int, match.groups()))


def git(root, *args):
    return subprocess.run(
        ["git", *args], cwd=root, check=True, text=True,
        encoding="utf-8", stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    ).stdout.strip()


def versions(root):
    manifest = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    if manifest["package"]["name"] != "sd2img":
        raise ValueError("This release script expects the sd2img package")
    current = manifest["package"]["version"]
    parse_version(current)
    lock = tomllib.loads((root / "Cargo.lock").read_text(encoding="utf-8"))
    packages = [entry for entry in lock["package"] if entry["name"] == "sd2img"]
    if len(packages) != 1 or packages[0]["version"] != current:
        raise ValueError("Cargo.toml and the sd2img Cargo.lock entry must match")
    return current


def replace_version(block, current, version):
    pattern = rf'(?m)^(version\s*=\s*"){re.escape(current)}("[^\r\n]*)$'
    updated, count = re.subn(pattern, lambda match: match[1] + version + match[2], block)
    if count != 1:
        raise ValueError("Expected exactly one package version declaration")
    return updated


def bump_files(root, version):
    current = versions(root)
    manifest_path = root / "Cargo.toml"
    manifest = manifest_path.read_text(encoding="utf-8")
    package = re.search(r"(?ms)^\[package\]\n.*?(?=^\[|\Z)", manifest)
    if package is None:
        raise ValueError("Missing [package] section")
    manifest = manifest[:package.start()] + replace_version(package[0], current, version) + manifest[package.end():]
    lock_path = root / "Cargo.lock"
    lock = lock_path.read_text(encoding="utf-8")
    replaced = 0

    def update_lock(match):
        nonlocal replaced
        block = match[0]
        if re.search(r'(?m)^name\s*=\s*"sd2img"\s*$', block):
            replaced += 1
            return replace_version(block, current, version)
        return block

    lock = re.sub(r"(?ms)^\[\[package\]\]\n.*?(?=^\[\[package\]\]|\Z)", update_lock, lock)
    if replaced != 1:
        raise ValueError("Expected exactly one sd2img lockfile entry")
    manifest_path.write_text(manifest, encoding="utf-8", newline="\n")
    lock_path.write_text(lock, encoding="utf-8", newline="\n")
    if versions(root) != version:
        raise ValueError("Version update verification failed")


def prepare(root, tag, main_ref="refs/remotes/origin/main"):
    if not tag.startswith("v"):
        raise ValueError("Release tags must start with v")
    version = tag[1:]
    parse_version(version)
    commit = git(root, "rev-parse", "--verify", f"refs/tags/{tag}^{{commit}}")
    if git(root, "rev-parse", "HEAD") != commit:
        raise ValueError("Checkout must match the release tag commit")
    if git(root, "status", "--porcelain", "--untracked-files=no"):
        raise ValueError("Tracked files must be clean before release preparation")
    ancestry = subprocess.run(
        ["git", "merge-base", "--is-ancestor", commit, main_ref],
        cwd=root, capture_output=True, text=True, encoding="utf-8",
    )
    if ancestry.returncode == 1:
        raise ValueError("Release tag commit must be contained in main")
    ancestry.check_returncode()
    # The pushed tag defines the executable version. Changes are local to this
    # build: source branches and tags are never rewritten by the workflow.
    bump_files(root, version)
    return {"version": version, "tag": tag}


def main():
    result = prepare(Path.cwd(), os.environ["GITHUB_REF_NAME"])
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8", newline="\n") as output:
        for key, value in result.items():
            output.write(f"{key}={value}\n")
    print(f"Prepared {result['tag']}")


if __name__ == "__main__":
    main()

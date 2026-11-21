import tempfile
from pathlib import Path
import unittest

from release import bump_files, git, prepare, versions


MANIFEST = '''[package]
name = "sd2img"
version = "0.1.0" # application
edition = "2021"

[dependencies]
other = "0.1.0"
'''
LOCK = '''version = 4

[[package]]
name = "other"
version = "0.1.0"

[[package]]
name = "sd2img"
version = "0.1.0"
dependencies = ["other"]
'''


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="sd2img-release-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "Cargo.toml").write_text(MANIFEST, encoding="utf-8")
        (self.root / "Cargo.lock").write_text(LOCK, encoding="utf-8")
        git(self.root, "init", "-b", "main")
        git(self.root, "config", "user.name", "Release test")
        git(self.root, "config", "user.email", "test@example.invalid")
        git(self.root, "config", "commit.gpgsign", "false")
        git(self.root, "config", "tag.gpgsign", "false")
        git(self.root, "add", "Cargo.toml", "Cargo.lock")
        git(self.root, "commit", "-m", "Source")
        self.source = git(self.root, "rev-parse", "HEAD")
        git(self.root, "checkout", "--detach", self.source)

    def test_build_uses_the_tag_version_without_changing_main_or_tag(self):
        git(self.root, "update-ref", "refs/remotes/origin/main", self.source)
        git(self.root, "tag", "v1.2.3")
        result = prepare(self.root, "v1.2.3")
        self.assertEqual(result, {"version": "1.2.3", "tag": "v1.2.3"})
        self.assertEqual(versions(self.root), "1.2.3")
        self.assertEqual(git(self.root, "rev-parse", "refs/heads/main"), self.source)
        self.assertEqual(git(self.root, "rev-parse", "v1.2.3^{commit}"), self.source)
        self.assertIn('version = "0.1.0"', git(self.root, "show", "v1.2.3:Cargo.toml"))

    def test_bump_changes_only_the_project_versions(self):
        bump_files(self.root, "1.2.3")
        self.assertEqual(versions(self.root), "1.2.3")
        self.assertIn('other = "0.1.0"', (self.root / "Cargo.toml").read_text())
        self.assertIn('name = "other"\nversion = "0.1.0"', (self.root / "Cargo.lock").read_text())
        self.assertIn('# application', (self.root / "Cargo.toml").read_text())

    def test_lockfile_mismatch_fails_before_writing(self):
        (self.root / "Cargo.lock").write_text(LOCK.replace('name = "sd2img"\nversion = "0.1.0"', 'name = "sd2img"\nversion = "0.0.9"'))
        with self.assertRaises(ValueError):
            bump_files(self.root, "1.2.3")
        self.assertEqual((self.root / "Cargo.toml").read_text(), MANIFEST)

    def test_invalid_or_prerelease_tags_are_rejected(self):
        for tag in ["1.2.3", "v1.2", "v01.2.3", "v1.2.3-beta", "v1.2.3+test", "--invalid"]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                prepare(self.root, tag)

    def test_tag_not_on_main_is_rejected_before_writing(self):
        git(self.root, "update-ref", "refs/remotes/origin/main", self.source)
        (self.root / "feature.txt").write_text("unmerged")
        git(self.root, "add", "feature.txt")
        git(self.root, "commit", "-m", "Unmerged feature")
        git(self.root, "tag", "v1.2.3")
        with self.assertRaisesRegex(ValueError, "contained in main"):
            prepare(self.root, "v1.2.3")
        self.assertEqual((self.root / "Cargo.toml").read_text(), MANIFEST)

    def test_annotated_tag_on_an_ancestor_of_main_is_accepted(self):
        git(self.root, "tag", "-a", "v1.2.3", "-m", "Release")
        (self.root / "later.txt").write_text("later main commit")
        git(self.root, "add", "later.txt")
        git(self.root, "commit", "-m", "Later main commit")
        git(self.root, "update-ref", "refs/remotes/origin/main", git(self.root, "rev-parse", "HEAD"))
        git(self.root, "checkout", "--detach", "v1.2.3")
        self.assertEqual(prepare(self.root, "v1.2.3")["version"], "1.2.3")

    def test_checkout_must_match_tag(self):
        git(self.root, "tag", "v1.2.3")
        (self.root / "later.txt").write_text("later")
        git(self.root, "add", "later.txt")
        git(self.root, "commit", "-m", "Later")
        git(self.root, "update-ref", "refs/remotes/origin/main", git(self.root, "rev-parse", "HEAD"))
        with self.assertRaisesRegex(ValueError, "Checkout must match"):
            prepare(self.root, "v1.2.3")

    def test_retry_prepares_the_same_version(self):
        git(self.root, "update-ref", "refs/remotes/origin/main", self.source)
        git(self.root, "tag", "v1.2.3")
        first = prepare(self.root, "v1.2.3")
        git(self.root, "restore", "Cargo.toml", "Cargo.lock")
        self.assertEqual(prepare(self.root, "v1.2.3"), first)


if __name__ == "__main__":
    unittest.main()

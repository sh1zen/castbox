"""Exercise the checked-in workflow scripts without contacting GitHub."""

import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import textwrap
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[1]


def workflow_script(filename, step_name):
    """Read a named step's literal run block using only the standard library."""
    lines = (ROOT / ".github" / "workflows" / filename).read_text(encoding="utf-8").splitlines()
    step = next(i for i, line in enumerate(lines) if line.strip() == "- name: " + step_name)
    for index in range(step + 1, len(lines)):
        line = lines[index]
        if line.lstrip().startswith("- name:"):
            break
        if line.strip() == "run: |":
            indent = len(line) - len(line.lstrip())
            end = index + 1
            while end < len(lines):
                following = lines[end]
                if following.strip() and len(following) - len(following.lstrip()) <= indent:
                    break
                end += 1
            return textwrap.dedent("\n".join(lines[index + 1:end])) + "\n"
    raise AssertionError(f"Missing literal run block: {filename}: {step_name}")


# This function shadows gh for every workflow invocation, so tests never use
# credentials or make API calls. Log each argument separately to check quoting.
FAKE_GH = r'''
gh() {
  printf '%s\t' "$@" >> "$GH_LOG"
  printf '\n' >> "$GH_LOG"
  if [[ -n "${GH_FAIL:-}" && "$*" == *"$GH_FAIL"* ]]; then
    return 42
  fi
  if [[ "$1 $2" == "release view" ]]; then
    [[ "${RELEASE_EXISTS:-false}" == true ]]
  elif [[ "$1" == api && "$*" == *--paginate* ]]; then
    printf '%s' "${DEPLOYMENT_IDS:-}"
  fi
}
'''


class WorkflowDiscoveryTests(unittest.TestCase):
    def test_ci_discovers_this_suite(self):
        ci = (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
        self.assertIn("python3 -m unittest discover -s tests -p 'test_workflows.py' -v", ci)
        self.assertTrue(workflow_script("release.yml", "Create source archives"))
        self.assertTrue(workflow_script("clear-deployments.yml", "Delete all deployments"))


class WorkflowScriptTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.bash = shutil.which("bash")
        if not cls.bash or not shutil.which("git"):
            if os.name == "nt":
                raise unittest.SkipTest("Workflow scripts require Bash and Git (CI runs them on Linux)")
            raise RuntimeError("Workflow scripts require Bash and Git on PATH")

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="castbox workflows ")
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.log = self.repo / "gh.log"
        self.env_file = self.repo / "github.env"
        self.env_file.touch()
        self.env = dict(os.environ, GITHUB_REF_NAME="v0.2.0",
                        GITHUB_REPOSITORY="example/castbox", GITHUB_SHA="abc123",
                        GITHUB_ACTOR="test-author", GITHUB_ENV=self.env_file.as_posix(),
                        GH_LOG=self.log.as_posix(), TMPDIR=self.repo.as_posix(),
                        GH_FAIL="", RELEASE_EXISTS="false", DEPLOYMENT_IDS="")

    def run_step(self, filename, name, *, prefix="", **env):
        script = workflow_script(filename, name)
        # Resolve the one Actions expression in the release shell block.
        script = script.replace("${{ github.repository }}", self.env["GITHUB_REPOSITORY"])
        self.assertNotIn("${{", script, "Unresolved GitHub Actions expression")
        return subprocess.run([self.bash, "--noprofile", "--norc", "-e", "-c",
                               FAKE_GH + prefix + script], cwd=self.repo,
                              env=dict(self.env, **env), text=True, encoding="utf-8", capture_output=True,
                              timeout=30)

    def assert_success(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def calls(self):
        if not self.log.exists():
            return []
        return [line.rstrip("\t").split("\t") for line in self.log.read_text().splitlines()]

    def init_repo(self):
        for args in (["init", "--quiet"], ["config", "core.autocrlf", "false"], ["add", "."],
                     ["-c", "user.name=Workflow Test", "-c", "user.email=test@example.com",
                      "-c", "commit.gpgsign=false",
                      "commit", "--quiet", "-m", "fixture"]):
            if args[0] == "add":
                (self.repo / "tracked.txt").write_bytes(b"committed content\n")
                (self.repo / ".gitignore").write_text("ignored.txt\ngithub.env\n")
            subprocess.run(["git", *args], cwd=self.repo, check=True,
                           capture_output=True, timeout=30)
        (self.repo / "tracked.txt").write_text("uncommitted content\n")
        (self.repo / "untracked.txt").write_text("not in HEAD\n")
        (self.repo / "ignored.txt").write_text("ignored\n")

    def test_archives_contain_only_committed_source(self):
        self.init_repo()
        result = self.run_step("release.yml", "Create source archives",
                               GITHUB_REF_NAME="feature/workflow fixes")
        self.assert_success(result)
        archive_env = dict(line.split("=", 1) for line in self.env_file.read_text().splitlines())
        self.assertEqual(archive_env["ARCHIVE_PREFIX"], "source-feature_workflow_fixes")
        archive_path = archive_env["ARCHIVE_DIR"]
        if os.name == "nt":
            archive_path = subprocess.run(
                [self.bash, "--noprofile", "--norc", "-c", 'cygpath -w "$1"', "--", archive_path],
                env=self.env, check=True, text=True, encoding="utf-8", capture_output=True,
                timeout=30).stdout.strip()
        archive_dir = Path(archive_path)
        prefix = archive_env["ARCHIVE_PREFIX"]
        with zipfile.ZipFile(archive_dir / (prefix + ".zip")) as archive:
            self.assertEqual(set(archive.namelist()), {"tracked.txt", ".gitignore"})
            self.assertEqual(archive.read("tracked.txt"), b"committed content\n")
        with tarfile.open(archive_dir / (prefix + ".tar.gz")) as archive:
            self.assertEqual(set(archive.getnames()), {"tracked.txt", ".gitignore"})
            self.assertEqual(archive.extractfile("tracked.txt").read(), b"committed content\n")

    def test_archive_failure_stops_before_exporting_assets(self):
        result = self.run_step("release.yml", "Create source archives",
                               prefix="git() { return 42; }\n")
        self.assertEqual(result.returncode, 42)
        self.assertEqual(self.env_file.read_text(), "")

    def test_release_notes_use_changelog(self):
        changelog = "# Changes\n\nA release note.\n"
        (self.repo / "CHANGELOG.md").write_text(changelog)
        self.assert_success(self.run_step("release.yml", "Prepare release notes"))
        self.assertEqual((self.repo / "RELEASE_BODY.md").read_text(), changelog)

    def test_release_notes_fallback_without_changelog(self):
        self.assert_success(self.run_step("release.yml", "Prepare release notes"))
        notes = (self.repo / "RELEASE_BODY.md").read_text()
        self.assertIn("## Release v0.2.0", notes)
        self.assertIn("https://github.com/example/castbox/commit/abc123", notes)
        self.assertIn("test-author", notes)
        self.assertRegex(notes, r"\d{4}-\d{2}-\d{2} .* UTC")

    def release(self, tag="v0.2.0", **env):
        return self.run_step("release.yml", "Create or update draft release",
                             GITHUB_REF_NAME=tag, ARCHIVE_DIR="/tmp/source archives",
                             ARCHIVE_PREFIX="source-test", **env)

    def test_new_stable_releases_are_drafts_and_not_prereleases(self):
        for tag in ("v0.2.0", "v1.2.3+build.5"):
            with self.subTest(tag=tag):
                self.log.unlink(missing_ok=True)
                self.assert_success(self.release(tag))
                calls = self.calls()
                self.assertEqual([call[:2] for call in calls],
                                 [["release", "view"], ["release", "create"], ["release", "upload"]])
                self.assertIn("--draft", calls[1])
                self.assertIn("--prerelease=false", calls[1])
                self.assertEqual(calls[1][calls[1].index("--target") + 1], "abc123")
                self.assertEqual(calls[2][3:5], ["/tmp/source archives/source-test.zip",
                                                "/tmp/source archives/source-test.tar.gz"])
                self.assertIn("--clobber", calls[2])

    def test_rc_and_branch_releases_are_prereleases(self):
        for tag in ("v1.2.3-rc.1", "main", "feature/fix", "v1.2.3junk"):
            with self.subTest(tag=tag):
                self.log.unlink(missing_ok=True)
                self.assert_success(self.release(tag))
                self.assertIn("--prerelease=true", self.calls()[1])

    def test_existing_release_is_edited_before_upload(self):
        self.assert_success(self.release(RELEASE_EXISTS="true"))
        calls = self.calls()
        self.assertEqual([call[:2] for call in calls],
                         [["release", "view"], ["release", "edit"], ["release", "upload"]])
        self.assertIn("--prerelease=false", calls[1])

    def test_release_create_or_edit_failure_prevents_upload(self):
        for exists, operation in (("false", "create"), ("true", "edit")):
            with self.subTest(operation=operation):
                self.log.unlink(missing_ok=True)
                result = self.release(RELEASE_EXISTS=exists, GH_FAIL="release " + operation)
                self.assertEqual(result.returncode, 42)
                self.assertEqual(len(self.calls()), 2)

    def test_release_upload_failure_is_propagated(self):
        self.assertEqual(self.release(GH_FAIL="release upload").returncode, 42)

    def cleanup_deployments(self, **env):
        return self.run_step("clear-deployments.yml", "Delete all deployments", **env)

    def test_cleanup_snapshots_all_pages_then_deactivates_before_deleting(self):
        ids = [str(i) for i in range(1, 206)]
        self.assert_success(self.cleanup_deployments(DEPLOYMENT_IDS="\n".join(ids) + "\n"))
        calls = self.calls()
        self.assertEqual(calls[0], ["api", "--paginate",
                                   "repos/example/castbox/deployments?per_page=100", "--jq", ".[].id"])
        self.assertEqual(len(calls), 1 + 2 * len(ids))
        for index, deployment_id in enumerate(ids):
            endpoint = f"repos/example/castbox/deployments/{deployment_id}"
            self.assertEqual(calls[1 + 2 * index],
                             ["api", "--method", "POST", endpoint + "/statuses",
                              "-f", "state=inactive", "--silent"])
            self.assertEqual(calls[2 + 2 * index],
                             ["api", "--method", "DELETE", endpoint, "--silent"])

    def test_cleanup_empty_list_makes_no_mutations(self):
        self.assert_success(self.cleanup_deployments())
        self.assertEqual(len(self.calls()), 1)

    def test_cleanup_api_failures_stop_processing(self):
        for failure, count in (("--paginate", 1), ("--method POST", 2), ("--method DELETE", 3)):
            with self.subTest(failure=failure):
                self.log.unlink(missing_ok=True)
                result = self.cleanup_deployments(DEPLOYMENT_IDS="11\n22\n", GH_FAIL=failure)
                self.assertEqual(result.returncode, 42)
                self.assertEqual(len(self.calls()), count)


if __name__ == "__main__":
    unittest.main()

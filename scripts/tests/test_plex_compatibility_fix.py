import importlib.util
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("fix", Path(__file__).parents[1] / "plex_compatibility_fix.py")
fix = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(fix)


class PatchGuardTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.repo = self.temp.name
        def git(*args):
            return subprocess.check_output(["git", "-C", self.repo, *args]).decode().strip()
        self.git = git
        git("init", "-q")
        path = Path(self.repo) / "rust/plex-pg-core/src/query.rs"
        path.parent.mkdir(parents=True)
        path.write_text("fn before() {}\n")
        (Path(self.repo) / "tests").mkdir()
        (Path(self.repo) / "tests/link.rs").symlink_to("../VERSION")
        git("add", ".")
        git("-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm", "fixture")
        self.req = {"base_sha": git("rev-parse", "HEAD"), "fingerprint": "a" * 64, "failure_category": "regression", "summary": "native assertion failed", "evidence_links": ["https://github.com/cgnl/plex-postgresql/actions/runs/1"]}
        self.patch = {"base_sha": self.req["base_sha"], "fingerprint": "a" * 64, "explanation": "Fix with causal regression test", "changes": [{"path": "rust/plex-pg-core/src/query.rs", "base_blob_sha": git("rev-parse", "HEAD:rust/plex-pg-core/src/query.rs"), "content": "fn after() {}\n"}]}

    def tearDown(self):
        self.temp.cleanup()

    def validate(self):
        return fix.validate(json.dumps(self.patch), self.req, self.repo)

    def test_legitimate_replacement_and_new_test(self):
        self.patch["changes"].append({"path": "tests/regression.rs", "base_blob_sha": None, "content": "#[test] fn regression() {}"})
        self.assertEqual(len(self.validate()["changes"]), 2)

    def test_forbidden_paths(self):
        for path in (".github/workflows/pwn.yml", "scripts/release.py", "rust/plex-pg-core/Cargo.toml", "tests/../VERSION", "/tests/x.rs", "tests//x.rs", "tests/./x.rs", "tests/x.rs\n", "tests/hook.sh"):
            with self.subTest(path=path):
                self.patch["changes"][0]["path"] = path
                with self.assertRaises(ValueError):
                    self.validate()

    def test_symlink_rejected(self):
        self.patch["changes"][0].update(path="tests/link.rs", base_blob_sha=self.git("rev-parse", "HEAD:tests/link.rs"))
        with self.assertRaises(ValueError):
            self.validate()

    def test_identity_and_blob_substitution(self):
        for key in ("base_sha", "fingerprint"):
            old = self.patch[key]
            self.patch[key] = "b" * len(old)
            with self.assertRaises(ValueError):
                self.validate()
            self.patch[key] = old
        self.patch["changes"][0]["base_blob_sha"] = "b" * 40
        with self.assertRaises(ValueError):
            self.validate()

    def test_binary_size_and_duplicate_rejected(self):
        for content in ("a\x00b", "x" * 100001):
            self.patch["changes"][0]["content"] = content
            with self.assertRaises(ValueError):
                self.validate()
        self.patch["changes"][0]["content"] = "valid"
        self.patch["changes"].append(dict(self.patch["changes"][0]))
        with self.assertRaises(ValueError):
            self.validate()

    def test_non_regression_and_untrusted_links_denied(self):
        self.req["failure_category"] = "infrastructure-failed"
        with self.assertRaises(ValueError):
            self.validate()
        self.req["failure_category"] = "regression"
        self.req["evidence_links"] = ["https://evil.example/run"]
        with self.assertRaises(ValueError):
            self.validate()

    def test_existing_pr_is_idempotent_without_writes(self):
        tree = []
        for line in self.git("ls-tree", "-r", "HEAD").splitlines():
            meta, path = line.split("\t")
            mode, kind, blob = meta.split()
            if path == self.patch["changes"][0]["path"]:
                content = self.patch["changes"][0]["content"].encode()
                blob = hashlib.sha1(b"blob " + str(len(content)).encode() + b"\x00" + content).hexdigest()
            tree.append({"mode": mode, "type": kind, "sha": blob, "path": path})
        responses = [[{"number": 7, "html_url": "https://github.com/example/repo/pull/7", "head": {"sha": "b" * 40}}], {"parents": [{"sha": self.req["base_sha"]}], "tree": {"sha": "d" * 40}}, {"tree": tree, "truncated": False}]
        with patch.object(fix, "api", side_effect=responses) as api, patch.object(fix, "git", side_effect=lambda *args: subprocess.check_output(["git", "-C", self.repo, *args])):
            result = fix.post(self.patch, self.req, "example/repo", "main", "fake-test-token")
        self.assertEqual(result["status"], "existing-fix-pr")
        self.assertEqual(api.call_count, 3)
        self.assertTrue(all(len(call.args) == 3 for call in api.call_args_list))

    def test_ci_dispatch_uses_only_allowlisted_trusted_workflows_and_exact_sha(self):
        def response(repository, endpoint, token, payload=None, method=None):
            if endpoint.endswith("?per_page=100"):
                return []
            return {"id": 10}
        with patch.object(fix, "api", side_effect=response) as api:
            result = fix.dispatch_ci("example/repo", "c" * 40, "main", "fake", 7, "a" * 64)
        dispatches = [call for call in api.call_args_list if call.args[1].endswith("/dispatches")]
        self.assertEqual([call.args[1] for call in dispatches], ["actions/workflows/ci.yml/dispatches", "actions/workflows/runtime-e2e.yml/dispatches"])
        for call in dispatches:
            self.assertEqual(call.args[3], {"ref": "main", "inputs": {"source_sha": "c" * 40}})
        self.assertEqual(result["ci_source_sha"], "c" * 40)
        with patch.object(fix, "api") as api:
            with self.assertRaises(ValueError):
                fix.dispatch_ci("example/repo", "branch", "main", "fake", 7, "a" * 64)
            api.assert_not_called()

    def test_partial_dispatch_failure_is_repaired_without_repeat_storm(self):
        comments = []
        calls = []
        fail_once = [True]
        def response(repository, endpoint, token, payload=None, method=None):
            if endpoint.endswith("?per_page=100"):
                return comments
            if endpoint == "issues/7/comments":
                comments.append({"id": 10, "user": {"login": "github-actions[bot]"}, "body": payload["body"]})
                return {"id": 10}
            if endpoint == "issues/comments/10":
                comments[0]["body"] = payload["body"]
                return {}
            calls.append(endpoint)
            if fail_once[0]:
                fail_once[0] = False
                raise RuntimeError("dispatch transport failure")
            return {}
        with patch.object(fix, "api", side_effect=response):
            with self.assertRaises(RuntimeError):
                fix.dispatch_ci("example/repo", "c" * 40, "main", "fake", 7, "a" * 64)
            fix.dispatch_ci("example/repo", "c" * 40, "main", "fake", 7, "a" * 64)
            fix.dispatch_ci("example/repo", "c" * 40, "main", "fake", 7, "a" * 64)
        self.assertEqual(calls, ["actions/workflows/ci.yml/dispatches", "actions/workflows/ci.yml/dispatches", "actions/workflows/runtime-e2e.yml/dispatches"])

    def test_dispatch_retry_budget_is_persistent_and_bounded(self):
        receipt = {"source_sha": "c" * 40, "fingerprint": "a" * 64, "workflows": {"ci.yml": {"attempts": 2, "status": "requesting"}}}
        comment = {"id": 10, "user": {"login": "github-actions[bot]"}, "body": "<!-- plex-compatibility-ci-v1 -->\n" + json.dumps(receipt)}
        with patch.object(fix, "api", return_value=[comment]) as api:
            with self.assertRaisesRegex(ValueError, "budget exhausted"):
                fix.dispatch_ci("example/repo", "c" * 40, "main", "fake", 7, "a" * 64)
        self.assertEqual(api.call_count, 1)

    def test_generation_workflow_boundaries(self):
        workflow = (Path(__file__).parents[2] / ".github/workflows/compatibility-fix.yml").read_text()
        generation, posting = workflow.split("  post:\n")
        self.assertNotIn("contents: write", generation)
        self.assertNotIn("pull-requests: write", generation)
        self.assertNotIn("packages: write", workflow)
        self.assertIn("persist-credentials: false", generation)
        self.assertIn("safety-strategy: drop-sudo", generation)
        self.assertIn("sandbox: workspace-write", generation)
        self.assertIn("bdf19a4a223ec2549a3e2274a0cf61556bc07675", generation)
        self.assertIn("needs.generate.outputs.patch", posting)
        self.assertNotIn("OPENAI_API_KEY", posting)
        self.assertNotIn("actions: write", generation)
        self.assertIn("actions: write", posting)
        ci = (Path(__file__).parents[2] / ".github/workflows/ci.yml").read_text()
        self.assertEqual(ci.count("uses: actions/cache@v4"), ci.count("if: github.event_name != 'workflow_dispatch' || inputs.source_sha == github.sha"))
        for filename in ("ci.yml", "runtime-e2e.yml"):
            ci = (Path(__file__).parents[2] / ".github/workflows" / filename).read_text()
            self.assertIn("contents: read", ci)
            self.assertNotIn("contents: write", ci)
            self.assertEqual(ci.count("uses: actions/checkout@v4"), ci.count("persist-credentials: false"))
            self.assertEqual(ci.count("uses: actions/checkout@v4"), ci.count("ref: ${{ inputs.source_sha || github.sha }}"))


if __name__ == "__main__":
    unittest.main()

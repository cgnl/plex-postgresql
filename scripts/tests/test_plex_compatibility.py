import copy
import contextlib
import io
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import subprocess
import os
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import plex_compatibility as compatibility


def snapshot(version="1.43.4.10903-e5521bd8c"):
    digest = "sha256:" + "a" * 64
    return {"schema_version": 1, "plex_version": version,
            "images": {name: {"ref": compatibility.IMAGE_REPOSITORIES[name] + "@" + digest, "digest": digest,
                              "platforms": {"amd64": digest, "arm64": digest}}
                       for name in compatibility.IMAGE_NAMES},
            "official_releases": {arch: {"url": "https://downloads.plex.tv/test.deb",
                                         "checksum": "b" * 40, "build": {"amd64": "linux-x86_64", "arm64": "linux-aarch64"}[arch]}
                                  for arch in compatibility.ARCHES}}


class CompatibilityTests(unittest.TestCase):
    def test_api_failures_are_visible_without_tokens(self):
        failure = subprocess.CalledProcessError(1, ['gh', 'api'], stderr='HTTP 403: PR creation disabled; private-token')
        output = io.StringIO()
        with patch.dict(os.environ, GH_TOKEN='private-token'), \
             patch.object(compatibility.subprocess, 'run', side_effect=failure), \
             contextlib.redirect_stderr(output), self.assertRaises(subprocess.CalledProcessError):
            compatibility.github_api('repos/owner/repo/pulls', 'POST', {})
        self.assertIn('HTTP 403: PR creation disabled', output.getvalue())
        self.assertNotIn('private-token', output.getvalue())

    def report_retry_responses(self, tree='report-tree'):
        source = self.plan['source_sha']
        return [[], {'default_branch': 'main'}, {'object': {'sha': source}},
                {'tree': {'sha': 'base-tree'}}, {'sha': 'report-tree'},
                {'object': {'sha': 'existing-report-commit'}},
                {'tree': {'sha': tree}, 'parents': [{'sha': source}]},
                {'html_url': 'https://github.com/owner/repo/pull/35'}]

    def test_report_retry_reuses_exact_existing_branch(self):
        with patch.object(compatibility, 'github_api', side_effect=self.report_retry_responses()) as api:
            url = compatibility.report_pr('owner/repo', self.plan, 'probe-passed',
                                          'https://github.com/owner/repo/actions/runs/12')
        self.assertTrue(url.endswith('/35'))
        self.assertFalse(any(call.args[0].endswith('/git/refs') for call in api.call_args_list))
        self.assertEqual(api.call_args_list[-1].args[0], 'repos/owner/repo/pulls')

    def test_report_retry_rejects_changed_branch(self):
        with patch.object(compatibility, 'github_api', side_effect=self.report_retry_responses('other-tree')) as api:
            with self.assertRaisesRegex(ValueError, 'does not match'):
                compatibility.report_pr('owner/repo', self.plan, 'probe-passed',
                                        'https://github.com/owner/repo/actions/runs/12')
        self.assertFalse(any(len(call.args) > 1 and call.args[1] == 'POST'
                             and call.args[0].endswith('/pulls') for call in api.call_args_list))

    def setUp(self):
        self.policy = compatibility.read_json(SCRIPTS.parent / ".github/plex-compatibility-policy.json")
        self.history = {"schema_version": 1, "profiles": []}
        self.plan = compatibility.discovery_plan(snapshot(), self.policy, self.history, "a" * 40, "b" * 64)

    def test_real_policy_is_fail_closed_bootstrap(self):
        compatibility.validate_policy(self.policy)
        self.assertNotIn("autoPublish", self.policy)
        self.assertNotIn("bootstrapCertified", self.policy)
        trust_path = SCRIPTS.parent / ".github/release-trust.json"
        self.assertTrue(not trust_path.exists() or compatibility.read_json(trust_path).get("enabled") is not True)
        self.assertEqual(compatibility.read_json(SCRIPTS.parent / ".github/plex-compatibility-profiles.json")["profiles"], [])
        self.assertEqual(self.plan["bootstrap"], "latest-only")
        self.assertEqual(self.plan["states"][0]["state"], "version-unverified")
        invalid = {**self.policy, "maxHistoricalProfiles": 3}
        with self.assertRaisesRegex(ValueError, "bounded"):
            compatibility.validate_policy(invalid)

    def test_fingerprint_ignores_observation_time_but_binds_source_and_fixture(self):
        observed = {**snapshot(), "observed_at": "tomorrow", "promotion_allowed": True}
        other = compatibility.discovery_plan(observed, self.policy, self.history, "a" * 40, "b" * 64)
        self.assertEqual(self.plan["fingerprint"], other["fingerprint"])
        changed = compatibility.discovery_plan(observed, self.policy, self.history, "c" * 40, "b" * 64)
        self.assertNotEqual(self.plan["fingerprint"], changed["fingerprint"])
        changed = compatibility.discovery_plan(observed, self.policy, self.history, "a" * 40, "c" * 64)
        self.assertNotEqual(self.plan["fingerprint"], changed["fingerprint"])

    def test_profiles_reject_mutable_refs_missing_architecture_and_tampering(self):
        for mutation in ("mutable", "architecture", "fingerprint", "package", "output-injection"):
            profile = compatibility.make_profile(snapshot())
            if mutation == "mutable":
                profile["images"]["plexinc"]["ref"] = "plexinc/pms-docker:latest"
            elif mutation == "architecture":
                del profile["images"]["plexinc"]["platforms"]["arm64"]
            elif mutation == "package":
                profile["official_releases"]["amd64"]["url"] = "https://attacker.test/file.deb"
            elif mutation == "output-injection":
                profile["images"]["plexinc"]["ref"] = "evil\nrelease_allowed=true\n@" + profile["images"]["plexinc"]["digest"]
            else:
                profile["fingerprint"] = "0" * 64
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                compatibility.validate_profile(profile)

    def test_latest_lag_never_claims_eligibility(self):
        profile = compatibility.make_profile(snapshot())
        measured = {variant: {arch: profile["plex_version"] for arch in compatibility.ARCHES}
                    for variant in ("plexinc", "linuxserver")}
        profile["measured_versions"] = measured
        self.assertEqual(compatibility.latest_state(profile), "eligible")
        measured["linuxserver"]["arm64"] = "1.42.0.10000-deadbeef"
        self.assertEqual(compatibility.latest_state(profile), "waiting-for-image")
        del measured["linuxserver"]["amd64"]
        with self.assertRaises(ValueError):
            compatibility.validate_profile(profile)

    def test_history_is_distinct_bounded_and_unavailability_visible(self):
        profiles = []
        for major in range(39, 43):
            profile = compatibility.make_profile(snapshot(f"1.{major}.0.10000-deadbeef"))
            profile["availability"] = "validated" if major == 39 else "unavailable"
            profiles.append(profile)
        plan = compatibility.discovery_plan(snapshot(), self.policy,
                                             {"schema_version": 1, "profiles": profiles}, "a" * 40, "b" * 64)
        self.assertEqual(plan["historical_coverage_count"], 2)
        self.assertEqual(plan["states"][2]["state"], "coverage-unavailable")
        self.assertFalse(plan["publication_allowed"])

    def test_duplicate_failure_and_success_do_not_repeat(self):
        for terminal in ("regression", "certified", "infrastructure-failed"):
            ledger, action = compatibility.enqueue(compatibility.empty_ledger(), self.plan)
            self.assertEqual(action, "start-probe")
            ledger = compatibility.finish(ledger, self.plan["fingerprint"], terminal,
                                           "https://github.com/cgnl/plex-postgresql/actions/runs/1")
            _, action = compatibility.enqueue(ledger, self.plan)
            self.assertEqual(action, "unchanged-" + terminal)

    def test_single_active_and_latest_pending_supersedes_only_pending(self):
        ledger, _ = compatibility.enqueue(compatibility.empty_ledger(), self.plan)
        pending = {**self.plan, "fingerprint": "c" * 64}
        ledger, action = compatibility.enqueue(ledger, pending)
        self.assertEqual(action, "queued")
        newer = {**self.plan, "fingerprint": "d" * 64}
        ledger, action = compatibility.enqueue(ledger, newer)
        self.assertEqual(ledger["active"], self.plan["fingerprint"])
        self.assertEqual(ledger["pending"], newer["fingerprint"])
        self.assertEqual(ledger["attempts"][pending["fingerprint"]]["state"], "superseded")

    def test_assertions_never_auto_retry_and_infra_at_most_once(self):
        ledger, _ = compatibility.enqueue(compatibility.empty_ledger(), self.plan)
        url = "https://github.com/cgnl/plex-postgresql/actions/runs/1"
        with self.assertRaises(ValueError):
            compatibility.finish(ledger, self.plan["fingerprint"], "regression", url, True)
        ledger = compatibility.finish(ledger, self.plan["fingerprint"], "infrastructure-failed", url, True)
        with self.assertRaises(ValueError):
            compatibility.finish(ledger, self.plan["fingerprint"], "infrastructure-failed", url, True)

    def test_completion_requires_active_ownership_and_trusted_run_link(self):
        ledger, _ = compatibility.enqueue(compatibility.empty_ledger(), self.plan)
        with self.assertRaises(ValueError):
            compatibility.finish(ledger, "c" * 64, "certified", "https://github.com/cgnl/plex-postgresql/actions/runs/1")
        with self.assertRaises(ValueError):
            compatibility.finish(ledger, self.plan["fingerprint"], "certified", "https://attacker.test/log")

    def test_reporting_does_not_invent_breaking_change_or_root_cause(self):
        for category in ("waiting-for-image", "infrastructure-failed", "insufficient-evidence"):
            self.assertIn("No breaking change is established", compatibility.report(self.plan, category))
            with self.assertRaises(ValueError):
                compatibility.fix_request(self.plan, category, "https://github.com/cgnl/plex-postgresql/actions/runs/1")
        request = compatibility.fix_request(self.plan, "regression", "https://github.com/cgnl/plex-postgresql/actions/runs/1")
        self.assertEqual(request["base_sha"], self.plan["source_sha"])
        self.assertEqual(request["failure_category"], "regression")
        self.assertIn("not been established", compatibility.report(self.plan, "regression"))

    def test_fixed_profile_cli_emits_exact_legacy_fields(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            compatibility.write_json(path / "profile.json", self.plan["profiles"][0])
            with patch.dict("os.environ", {"GITHUB_OUTPUT": str(path / "outputs")}), patch.object(
                    sys, "argv", ["plex_compatibility.py", "validate-profile", "--input",
                                  str(path / "profile.json"), "--output", str(path / "candidate.json")]):
                compatibility.main()
            self.assertEqual(compatibility.read_json(path / "candidate.json"), self.plan["profiles"][0])
            output = (path / "outputs").read_text()
            self.assertIn("plex_version=" + snapshot()["plex_version"], output)
            self.assertIn("postgres18_image=", output)

    def missing(self):
        return subprocess.CalledProcessError(1, ["gh"], stderr="gh: Not Found (HTTP 404)")

    def test_ledger_migrates_existing_issue_readonly(self):
        ledger, _ = compatibility.enqueue(compatibility.empty_ledger(), self.plan)
        issue = {"title": "Plex compatibility automation ledger", "number": 34,
                 "body": compatibility.ledger_body(ledger)}
        with patch.object(compatibility, "github_api", side_effect=[self.missing(), self.missing(), issue]) as api:
            sha, restored = compatibility.ledger_state("cgnl/plex-postgresql")
        self.assertIsNone(sha)
        self.assertEqual(restored, ledger)
        self.assertIn("ref=automation%2Fcompatibility-state", api.call_args_list[0].args[0])
        self.assertEqual(api.call_args_list[2].args, ("repos/cgnl/plex-postgresql/issues/34",))

    def test_state_roundtrip_preserves_terminal_dedup_and_sha(self):
        import base64
        ledger = compatibility.empty_ledger()
        ledger["attempts"]["c" * 64] = {"state": "regression", "plan": self.plan}
        with patch.object(compatibility, "github_api", side_effect=[{"default_branch": "main"}, {}, None]) as api:
            compatibility.persist_ledger("cgnl/plex-postgresql", "d" * 40, ledger)
        path, method, payload = api.call_args.args
        self.assertEqual(method, "PUT")
        self.assertEqual(payload["branch"], compatibility.LEDGER_BRANCH)
        self.assertEqual(payload["sha"], "d" * 40)
        envelope = json.loads(base64.b64decode(payload["content"]))
        self.assertEqual(envelope["ledger"]["attempts"]["c" * 64], {"state": "regression"})
        content = {"type": "file", "encoding": "base64", "sha": "e" * 40,
                   "content": payload["content"]}
        with patch.object(compatibility, "github_api", return_value=content) as api:
            sha, restored = compatibility.ledger_state("cgnl/plex-postgresql")
        self.assertEqual(sha, "e" * 40)
        self.assertEqual(restored, envelope["ledger"])
        self.assertEqual(api.call_count, 1)

    def test_first_write_creates_only_dedicated_branch(self):
        with patch.object(compatibility, "github_api", side_effect=[{"default_branch": "main"}, self.missing(), {"object": {"sha": "a" * 40}}, None, None]) as api:
            compatibility.persist_ledger("cgnl/plex-postgresql", None, compatibility.empty_ledger())
        ref = api.call_args_list[3].args[2]
        self.assertEqual(ref, {"ref": "refs/heads/automation/compatibility-state", "sha": "a" * 40})
        self.assertNotIn("sha", api.call_args.args[2])
        self.assertTrue(all("/issues" not in call.args[0] for call in api.call_args_list))

    def test_foreign_or_malformed_state_never_falls_back_to_issue(self):
        import base64
        for envelope in ({"marker": "foreign", "ledger": compatibility.empty_ledger()},
                         {"marker": compatibility.LEDGER_MARKER, "ledger": {}},
                         {"marker": compatibility.LEDGER_MARKER, "ledger": {**compatibility.empty_ledger(), "active": "a" * 64}}):
            content = {"type": "file", "encoding": "base64", "sha": "e" * 40,
                       "content": base64.b64encode(json.dumps(envelope).encode()).decode()}
            with patch.object(compatibility, "github_api", return_value=content) as api, self.assertRaises(ValueError):
                compatibility.ledger_state("cgnl/plex-postgresql")
            self.assertEqual(api.call_count, 1)

    def test_initialization_race_reads_existing_ref_without_forcing_update(self):
        collision = subprocess.CalledProcessError(1, ["gh"], stderr="HTTP 422")
        with patch.object(compatibility, "github_api", side_effect=[{"default_branch": "main"},
                self.missing(), {"object": {"sha": "a" * 40}}, collision, {}, None]) as api:
            compatibility.persist_ledger("cgnl/plex-postgresql", None, compatibility.empty_ledger())
        self.assertEqual(api.call_args_list[4].args, ("repos/cgnl/plex-postgresql/git/ref/heads/automation/compatibility-state",))
        self.assertNotIn("sha", api.call_args.args[2])
        self.assertTrue(all(call.args[1] != "PATCH" for call in api.call_args_list if len(call.args) > 1))

    def test_existing_state_branch_missing_file_is_not_reinitialized(self):
        with patch.object(compatibility, "github_api", side_effect=[self.missing(), {}]) as api, self.assertRaisesRegex(ValueError, "no ledger"):
            compatibility.ledger_state("cgnl/plex-postgresql")
        self.assertEqual(api.call_count, 2)

    def test_state_branch_cannot_be_default_branch(self):
        with patch.object(compatibility, "github_api", return_value={"default_branch": compatibility.LEDGER_BRANCH}) as api, self.assertRaisesRegex(ValueError, "default branch"):
            compatibility.persist_ledger("cgnl/plex-postgresql", None, compatibility.empty_ledger())
        self.assertEqual(api.call_count, 1)

    def test_state_conflict_is_not_retried_or_overwritten(self):
        conflict = subprocess.CalledProcessError(1, ["gh"], stderr="HTTP 409")
        with patch.object(compatibility, "github_api", side_effect=[{"default_branch": "main"}, {}, conflict]) as api, self.assertRaises(subprocess.CalledProcessError):
            compatibility.persist_ledger("cgnl/plex-postgresql", "d" * 40, compatibility.empty_ledger())
        self.assertEqual(api.call_count, 3)

    def observation(self, category="probe-passed"):
        return {"schema_version": 1, "candidate_source_sha": self.plan["source_sha"],
                "profile_fingerprint": self.plan["profiles"][0]["fingerprint"],
                "profile_id": self.plan["latest_profile_id"], "mode": "probe",
                "run_id": "123", "run_attempt": 2, "category": category}

    def test_typed_observation_requires_exact_run_attempt_source_and_profile(self):
        self.assertEqual(compatibility.classify_observation(self.plan, None, 123, 2), "insufficient-evidence")
        self.assertEqual(compatibility.classify_observation(self.plan, self.observation(), 123, 2), "probe-passed")
        for key, invalid in (("run_id", "124"), ("run_attempt", 1),
                             ("candidate_source_sha", "c" * 40), ("mode", "full"),
                             ("profile_fingerprint", "d" * 64)):
            observation = {**self.observation(), key: invalid}
            with self.subTest(key=key), self.assertRaises(ValueError):
                compatibility.classify_observation(self.plan, observation, 123, 2)

    def test_regression_requires_fresh_same_leaf_historical_comparison(self):
        observation = self.observation("regression")
        self.assertEqual(compatibility.classify_observation(self.plan, observation, 123, 2), "insufficient-evidence")
        historical = compatibility.make_profile(snapshot("1.42.0.10000-deadbeef"))
        plan = {**self.plan, "profiles": self.plan["profiles"] + [historical]}
        observation["baseline"] = {"candidate_source_sha": plan["source_sha"],
                                   "profile_fingerprint": historical["fingerprint"], "outcome": "passed", "fresh": True}
        self.assertEqual(compatibility.classify_observation(plan, observation, 123, 2), "regression")
        observation["baseline"]["candidate_source_sha"] = "c" * 40
        self.assertEqual(compatibility.classify_observation(plan, observation, 123, 2), "insufficient-evidence")

    def test_followups_only_green_staging_or_proven_regression(self):
        url = "https://github.com/cgnl/plex-postgresql/actions/runs/123"
        for category, workflow in (("probe-passed", "stage-compatibility-release.yml"),
                                   ("regression", "compatibility-fix.yml")):
            with patch.object(compatibility, "github_api", side_effect=[{"default_branch": "main"},
                    {"object": {"sha": self.plan["source_sha"]}}, None]) as api:
                self.assertEqual(compatibility.dispatch_followup("cgnl/plex-postgresql", self.plan, category, url), workflow)
                self.assertIn(workflow, api.call_args.args[0])
        with patch.object(compatibility, "github_api", side_effect=[{"default_branch": "main"},
                {"object": {"sha": self.plan["source_sha"]}}]) as api:
            self.assertEqual(compatibility.dispatch_followup("cgnl/plex-postgresql", self.plan, "waiting-for-image", url), "no-followup")
            self.assertEqual(api.call_count, 2)
        with patch.object(compatibility, "github_api", side_effect=[{"default_branch": "main"},
                {"object": {"sha": "c" * 40}}]), self.assertRaises(ValueError):
            compatibility.dispatch_followup("cgnl/plex-postgresql", self.plan, "probe-passed", url)

    def test_real_producer_and_monitor_cli_route_fresh_baseline_regression(self):
        historical = compatibility.make_profile(snapshot("1.42.0.10000-deadbeef"))
        plan = {**self.plan, "profiles": self.plan["profiles"] + [historical], "historical_coverage_count": 1}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            compatibility.write_json(root / "plan.json", plan)
            for index, profile in enumerate(plan["profiles"]):
                evidence = root / ("evidence-" + profile["id"])
                for variant in ("linuxserver", "plexinc"):
                    for arch in ("amd64", "arm64"):
                        for pg in (15, 18):
                            lane = evidence / f"{profile['id']}-candidate-{variant}-{arch}-pg{pg}"
                            (lane / "container").mkdir(parents=True)
                            (lane / "source-sha.txt").write_text(plan["source_sha"])
                            (lane / "postgres-ref.txt").write_text(profile["images"][f"postgres{pg}"]["ref"])
                            (lane / "container/installed-version.txt").write_text(profile["plex_version"])
                            compatibility.write_json(lane / "container/result.json", {
                                "variant": variant, "arch": arch, "plex_version": profile["plex_version"],
                                "exit_code": 1 if index == 0 else 0, "workload_passed": index != 0,
                                "phase": "live-postgres-recovery" if index == 0 else "native-workload-complete"})
                output = root / "observations" / (profile["id"] + "-compatibility-observation") / "observation.json"
                environment = {**os.environ, "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "2", "GITHUB_OUTPUT": str(root / "outputs")}
                subprocess.run([sys.executable, str(SCRIPTS / "native_compatibility_observation.py"),
                    "--root", str(evidence), "--profile-json", json.dumps(profile), "--source", plan["source_sha"],
                    "--mode", "probe", "--jobs-passed", "false" if index == 0 else "true", "--output", str(output)],
                    env=environment, text=True, capture_output=True, check=True)
            command = [sys.executable, str(SCRIPTS / "plex_compatibility.py"), "observe", "--plan", str(root / "plan.json"),
                       "--observations-root", str(root / "observations"), "--run-id", "123", "--run-attempt", "2"]
            result = subprocess.run(command, capture_output=True, text=True, check=True)
            self.assertEqual(result.stdout.strip(), "regression")
            baseline_path = root / "observations" / (historical["id"] + "-compatibility-observation") / "observation.json"
            baseline = compatibility.read_json(baseline_path)
            baseline["candidate_source_sha"] = "c" * 40
            compatibility.write_json(baseline_path, baseline)
            rejected = subprocess.run(command, capture_output=True, text=True)
            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn("source", rejected.stderr)
            baseline_path.unlink()
            result = subprocess.run(command, capture_output=True, text=True, check=True)
            self.assertEqual(result.stdout.strip(), "insufficient-evidence")
        workflow = (SCRIPTS.parent / ".github/workflows/check-updates.yml").read_text()
        self.assertIn("pattern: '*-compatibility-observation'", workflow)
        self.assertIn("--observations-root observation", workflow)
        self.assertIn("dispatch-followup", workflow)

    def test_monitor_declares_payload_dependency_instead_of_matrix_order(self):
        workflow = (SCRIPTS.parent / ".github/workflows/check-updates.yml").read_text()
        latest = workflow.split("\n  probe:\n", 1)[1].split("\n  historical-probe:\n", 1)[0]
        historical = workflow.split("\n  historical-probe:\n", 1)[1].split("\n  record:\n", 1)[0]
        record = workflow.split("\n  record:\n", 1)[1]
        self.assertNotIn("matrix:", latest)
        self.assertIn("needs: [discover, claim, probe]", historical)
        self.assertIn("if: always() && needs.claim.outputs.action == 'start-probe'", historical)
        self.assertIn("max-parallel: 1", historical)
        self.assertIn("payload_profile_id: ${{ needs.discover.outputs.profile_id }}", historical)
        self.assertIn("json.dumps(profiles[1:]", workflow)
        self.assertIn("needs: [discover, claim, probe, historical-probe]", record)


class ResolverTests(unittest.TestCase):
    def setUp(self):
        spec = importlib.util.spec_from_file_location("upstream", SCRIPTS / "check-upstream-updates.py")
        self.resolver = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.resolver)

    def test_registry_digest_verified_and_both_architectures_required(self):
        import hashlib
        manifest = {"manifests": [{"platform": {"os": "linux", "architecture": arch},
                                   "digest": "sha256:" + "a" * 64} for arch in ("amd64", "arm64")]}
        payload = json.dumps(manifest).encode()
        responses = [(b'{"token":"test-token"}', {}),
                     (payload, {"Docker-Content-Digest": "sha256:" + hashlib.sha256(payload).hexdigest()})]
        with patch.object(self.resolver, "fetch", side_effect=responses):
            image = self.resolver.resolve_image("plexinc/pms-docker", "latest")
        self.assertEqual(set(image["platforms"]), {"amd64", "arm64"})
        responses[1] = (payload, {"Docker-Content-Digest": "sha256:" + "b" * 64})
        with patch.object(self.resolver, "fetch", side_effect=responses), self.assertRaises(ValueError):
            self.resolver.resolve_image("plexinc/pms-docker", "latest")


if __name__ == "__main__":
    unittest.main()

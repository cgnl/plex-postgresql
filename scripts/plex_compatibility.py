#!/usr/bin/env python3
"""Immutable compatibility profiles, bounded attempt ledger and factual reporting.

This module grants no publication authority. A profile is an immutable input
description, not proof that those inputs passed native acceptance.
"""

import argparse
import base64
import hashlib
import json
import re
import os
import subprocess
from pathlib import Path
from urllib.parse import quote

SHA = re.compile(r"[0-9a-f]{40}\Z")
DIGEST = re.compile(r"sha256:[0-9a-f]{64}\Z")
VERSION = re.compile(r"[0-9]+(?:\.[0-9]+){3}-[0-9a-f]+\Z")
ID = re.compile(r"[A-Za-z0-9-]+\Z")
IMAGE_NAMES = {"linuxserver", "plexinc", "builder", "postgres", "postgres15", "postgres18"}
IMAGE_REPOSITORIES = {"linuxserver": "linuxserver/plex", "plexinc": "plexinc/pms-docker",
                      "builder": "library/alpine", "postgres": "library/postgres",
                      "postgres15": "library/postgres", "postgres18": "library/postgres"}
ARCHES = {"amd64", "arm64"}
TERMINAL = {"probe-passed", "certified", "published", "regression", "infrastructure-failed", "insufficient-evidence", "waiting-for-image"}


def fingerprint(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"),
                                     ensure_ascii=True).encode()).hexdigest()


def read_json(path):
    return json.loads(Path(path).read_text())


def write_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def validate_policy(policy):
    if policy.get("schema_version") != 1:
        raise ValueError("Unsupported compatibility policy")
    bounds = {"maxHistoricalProfiles": 2, "maxActiveCertifications": 1,
              "maxPendingFingerprints": 1, "maxAutomaticInfrastructureRetries": 1,
              "maxAutomaticAssertionRetries": 0}
    for name, maximum in bounds.items():
        value = policy.get(name)
        if type(value) is not int or not 0 <= value <= maximum:
            raise ValueError(f"Invalid bounded policy: {name}")
    if policy["maxActiveCertifications"] != 1 or policy["maxPendingFingerprints"] != 1:
        raise ValueError("Exactly one active and one pending slot are supported")
    if policy.get("historyUnavailablePolicy") != "report-and-block":
        raise ValueError("Unavailable required history cannot be silently excluded")
    if policy.get("platforms") != ["linux/amd64", "linux/arm64"]:
        raise ValueError("Both native Linux architectures are required")
    if set(policy.get("variants", [])) != {"plexinc", "linuxserver"}:
        raise ValueError("Both Plex container variants are required")
    return policy


def profile_inputs(profile):
    # Measured versions are evidence, not input identity. Timestamps and API
    # response formatting must never launch an otherwise unchanged build.
    return {name: profile[name] for name in
            ("schema_version", "plex_version", "images", "official_releases")}


def validate_profile(profile):
    if profile.get("schema_version") != 1 or not ID.fullmatch(profile.get("id", "")):
        raise ValueError("Invalid profile schema or id")
    if not VERSION.fullmatch(profile.get("plex_version", "")):
        raise ValueError("Invalid Plex version")
    if set(profile.get("images", {})) != IMAGE_NAMES:
        raise ValueError("Profile must freeze every runtime and builder image")
    for name, image in profile["images"].items():
        if not DIGEST.fullmatch(image.get("digest", "")):
            raise ValueError("Invalid image digest")
        if image.get("ref") != IMAGE_REPOSITORIES[name] + "@" + image["digest"]:
            raise ValueError("Image reference must bind its immutable digest")
        if set(image.get("platforms", {})) != ARCHES or not all(
                DIGEST.fullmatch(value) for value in image["platforms"].values()):
            raise ValueError("Both immutable architecture manifests are required")
    if set(profile.get("official_releases", {})) != ARCHES:
        raise ValueError("Both official packages are required")
    for arch, release in profile["official_releases"].items():
        from urllib.parse import urlparse
        url = urlparse(release.get("url", ""))
        if url.scheme != "https" or url.hostname != "downloads.plex.tv" or url.username or url.password:
            raise ValueError("Untrusted package URL")
        if not re.fullmatch(r"[0-9a-fA-F]{40}|[0-9a-fA-F]{64}", release.get("checksum", "")):
            raise ValueError("Official package checksum is missing or invalid")
        if release.get("build") != {"amd64": "linux-x86_64", "arm64": "linux-aarch64"}[arch]:
            raise ValueError("Official package architecture mismatch")
    expected = fingerprint(profile_inputs(profile))
    if profile.get("fingerprint") != expected:
        raise ValueError("Profile input fingerprint mismatch")
    measured = profile.get("measured_versions", {})
    if measured and (set(measured) != {"plexinc", "linuxserver"} or any(
            set(versions) != ARCHES or any(not VERSION.fullmatch(v) for v in versions.values())
            for versions in measured.values())):
        raise ValueError("Measured versions must cover all four native image cells")
    return profile


def make_profile(snapshot):
    profile = {name: snapshot[name] for name in
               ("schema_version", "plex_version", "images", "official_releases")}
    profile["fingerprint"] = fingerprint(profile)
    profile["id"] = "plex-" + profile["fingerprint"][:16]
    return validate_profile(profile)


def emit_profile(profile, output):
    validate_profile(profile)
    write_json(output, profile)
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a") as stream:
            stream.write(f"plex_version={profile['plex_version']}\nprofile_id={profile['id']}\n")
            stream.write(f"profile_fingerprint={profile['fingerprint']}\n")
            for name, image in profile["images"].items():
                stream.write(f"{name}_image={image['ref']}\n")


def latest_state(profile):
    measured = profile.get("measured_versions")
    if not measured:
        return "version-unverified"
    if any(version != profile["plex_version"] for versions in measured.values()
           for version in versions.values()):
        return "waiting-for-image"
    return "eligible"


def discovery_plan(snapshot, policy, history, source_sha, fixture_hash):
    validate_policy(policy)
    if not SHA.fullmatch(source_sha) or not re.fullmatch(r"[0-9a-f]{64}", fixture_hash):
        raise ValueError("Discovery requires exact source and fixture hashes")
    if history.get("schema_version") != 1 or not isinstance(history.get("profiles"), list):
        raise ValueError("Invalid historical profile ledger")
    latest = make_profile(snapshot)
    profiles = [latest]
    states = [{"profile_id": latest["id"], "state": latest_state(latest)}]
    seen_versions = {latest["plex_version"]}
    # Ledger order is deliberate maintainer policy, not a guess at old tags.
    for historical in history["profiles"]:
        validate_profile(historical)
        if historical["plex_version"] in seen_versions:
            continue
        if tuple(map(int, historical["plex_version"].split("-")[0].split("."))) >= tuple(
                map(int, latest["plex_version"].split("-")[0].split("."))):
            raise ValueError("Historical profiles must describe an earlier public Plex release")
        if len(profiles) - 1 >= policy["maxHistoricalProfiles"]:
            break
        seen_versions.add(historical["plex_version"])
        profiles.append(historical)
        available = historical.get("availability") == "validated"
        state = "eligible" if available else "coverage-unavailable"
        states.append({"profile_id": historical["id"], "state": state})
    policy_hash = fingerprint(policy)
    identity = {"source_sha": source_sha, "policy_hash": policy_hash,
                "fixture_hash": fixture_hash,
                "profiles": [p["fingerprint"] for p in profiles]}
    return {"schema_version": 1, **identity, "fingerprint": fingerprint(identity),
            "latest_profile_id": latest["id"], "profiles": profiles, "states": states,
            "changed": True, "bootstrap": "latest-only" if len(profiles) == 1 else "historical-inputs-selected",
            "historical_coverage_count": len(profiles) - 1,
            "publication_allowed": False}


def empty_ledger():
    return {"schema_version": 1, "active": None, "pending": None, "attempts": {}, "superseded": []}


def enqueue(ledger, plan, retry=False):
    """Pure bounded state transition; callers persist under one serialized lock."""
    ledger = json.loads(json.dumps(ledger))
    if ledger.get("schema_version") != 1 or not isinstance(ledger.get("attempts"), dict):
        raise ValueError("Invalid attempt ledger")
    key = plan["fingerprint"]
    if not re.fullmatch(r"[0-9a-f]{64}", key):
        raise ValueError("Invalid discovery fingerprint")
    attempt = ledger["attempts"].get(key)
    if ledger.get("active") == key:
        return ledger, "already-active"
    if ledger.get("pending") == key:
        return ledger, "already-pending"
    if attempt and not retry:
        return ledger, "unchanged-" + attempt["state"]
    if ledger.get("active"):
        old_pending = ledger.get("pending")
        if old_pending:
            ledger["attempts"][old_pending]["state"] = "superseded"
            ledger["superseded"].append(old_pending)
            ledger["superseded"] = ledger["superseded"][-32:]
        ledger["pending"] = key
        ledger["attempts"][key] = {"state": "pending", "plan": plan, "automatic_retries": 0}
        return ledger, "queued"
    ledger["active"] = key
    if ledger.get("pending") and ledger["pending"] != key:
        ledger["attempts"][ledger["pending"]]["state"] = "superseded"
        ledger["superseded"] = (ledger["superseded"] + [ledger["pending"]])[-32:]
    ledger["pending"] = None
    ledger["attempts"][key] = {"state": "active", "plan": plan,
                               "automatic_retries": attempt.get("automatic_retries", 0) if attempt else 0}
    return ledger, "start-probe"


def finish(ledger, key, state, evidence_url, automatic_retry=False):
    ledger = json.loads(json.dumps(ledger))
    if ledger.get("active") != key or state not in TERMINAL:
        raise ValueError("Completion must match the active attempt and a terminal state")
    if not re.fullmatch(r"https://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/actions/runs/[0-9]+", evidence_url):
        raise ValueError("Completion requires a trusted workflow run link")
    attempt = ledger["attempts"][key]
    attempt.update(state=state, evidence_url=evidence_url)
    if automatic_retry:
        if state != "infrastructure-failed" or attempt["automatic_retries"] >= 1:
            raise ValueError("Only one infrastructure retry is permitted; assertions are never retried automatically")
        attempt["automatic_retries"] += 1
        attempt["state"] = "active"
        return ledger
    ledger["active"] = None
    return ledger


def report(plan, category, run_url=None):
    categories = {"version-unverified", "waiting-for-image", "coverage-unavailable", "probe-passed",
                  "regression", "infrastructure-failed", "insufficient-evidence", "certified"}
    if category not in categories:
        raise ValueError("Unknown factual report category")
    if run_url and not re.fullmatch(r"https://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/actions/runs/[0-9]+", run_url):
        raise ValueError("Report evidence must be a GitHub workflow run URL")
    lines = ["## Plex compatibility observation", "",
             f"Fingerprint: `{plan['fingerprint']}`", "",
             f"Candidate source: `{plan['source_sha']}`", "",
             f"Result: **{category}**.", "",
             f"Requested Plex versions: {', '.join(p['plex_version'] for p in plan['profiles'])}.", "",
             f"Historical input profiles selected: {plan['historical_coverage_count']} (maximum two).", ""]
    if category == "regression":
        lines.append("A reproducible native compatibility regression was observed. The root cause has not been established by this report.")
    elif category == "certified":
        lines.append("Certificate validation is required separately before any publication; this report grants no release authority.")
    else:
        lines.append("No breaking change is established by this observation. Compatibility and publication remain unproven.")
    if run_url:
        lines.extend(["", f"[Workflow evidence and logs]({run_url})."])
    lines.extend(["", "This report never merges a generated runtime fix automatically.", ""])
    return "\n".join(lines)


def validate_observation_binding(plan, profile, observation, run_id, run_attempt):
    expected = {"schema_version": 1, "candidate_source_sha": plan["source_sha"],
                "profile_fingerprint": profile["fingerprint"],
                "profile_id": profile["id"], "mode": "probe",
                "run_id": str(run_id), "run_attempt": int(run_attempt)}
    for name, value in expected.items():
        if observation.get(name) != value:
            raise ValueError(f"Observation binding mismatch: {name}")


def classify_observation(plan, observation, run_id, run_attempt):
    """Consume a typed observation from a trusted same-run producer.

    The caller downloads only this workflow run's named artifact. Raw failure
    logs, job conclusions and upstream JSON cannot grant regression authority.
    """
    if not observation:
        return "insufficient-evidence"
    validate_observation_binding(plan, plan["profiles"][0], observation, run_id, run_attempt)
    category = observation.get("category")
    if category not in {"probe-passed", "regression", "waiting-for-image",
                         "infrastructure-failed", "insufficient-evidence"}:
        raise ValueError("Unknown observation category")
    if category == "regression":
        baseline = observation.get("baseline", {})
        history = {profile["fingerprint"] for profile in plan["profiles"][1:]}
        if (baseline.get("candidate_source_sha") != plan["source_sha"]
                or baseline.get("profile_fingerprint") not in history
                or baseline.get("outcome") != "passed" or baseline.get("fresh") is not True):
            return "insufficient-evidence"
    return category


def classify_selected_observations(plan, root, run_id, run_attempt):
    """Compare independently produced latest/history observations from this run.

    Every selected profile has a unique artifact namespace. A passing old
    source's observation, a prior attempt or generic runner failure cannot
    serve as a fresh compatibility baseline.
    """
    root = Path(root)
    observations = {}
    for profile in plan["profiles"]:
        path = root / (profile["id"] + "-compatibility-observation") / "observation.json"
        if not path.is_file():
            continue
        observation = read_json(path)
        validate_observation_binding(plan, profile, observation, run_id, run_attempt)
        observations[profile["id"]] = observation
    latest = observations.get(plan["latest_profile_id"])
    if not latest:
        return "insufficient-evidence"
    if latest.get("category") != "compatibility-assertion-failed":
        category = classify_observation(plan, latest, run_id, run_attempt)
        # All required historical probes must pass before staging, even when
        # latest passes. Missing history never silently disappears.
        if category == "probe-passed" and any(
                observations.get(profile["id"], {}).get("category") != "probe-passed"
                for profile in plan["profiles"][1:]):
            return "insufficient-evidence"
        return category
    required = {f"{plan['latest_profile_id']}-candidate-{variant}-{arch}-pg{pg}"
                for variant in ("plexinc", "linuxserver") for arch in ("amd64", "arm64") for pg in (15, 18)}
    failed = latest.get("failed_cells", [])
    assertion_phases = {"api-write-routing", "native-media-scan", "real-movie-and-tv-library",
                        "native-import-and-rollback", "watch-state", "concurrent-real-media",
                        "restart", "native-workload-soak", "watch-state-reset",
                        "live-postgres-interruption", "live-postgres-recovery",
                        "postgres-unavailable", "postgres-recovery"}
    if (set(latest.get("observed_cells", [])) != required or latest.get("version_mismatches")
            or not failed or any(cell.get("cell") not in required or cell.get("phase") not in assertion_phases for cell in failed)):
        return "insufficient-evidence"
    for profile in plan["profiles"][1:]:
        baseline = observations.get(profile["id"])
        if baseline and baseline.get("category") == "probe-passed":
            latest = {**latest, "category": "regression", "baseline": {
                "candidate_source_sha": plan["source_sha"], "profile_fingerprint": profile["fingerprint"],
                "outcome": "passed", "fresh": True}}
            return classify_observation(plan, latest, run_id, run_attempt)
    return "insufficient-evidence"


def fix_request(plan, category, run_url):
    if category != "regression":
        raise ValueError("Only a reproducible regression can request a code-fix PR")
    # Reuse report's trusted-link validation. Never paste arbitrary logs into a
    # privileged orchestration input or infer a diagnosis from runner status.
    report(plan, category, run_url)
    return {"fingerprint": plan["fingerprint"], "base_sha": plan["source_sha"],
            "failure_category": "regression", "evidence_links": [run_url],
            "summary": "Native compatibility assertions failed; root cause requires investigation."}


def github_api(path, method="GET", body=None):
    args = ["gh", "api", path, "--method", method]
    if body is not None:
        args += ["--input", "-"]
    result = subprocess.run(args, input=json.dumps(body) if body is not None else None,
                            text=True, capture_output=True, check=True)
    return json.loads(result.stdout) if result.stdout.strip() else None


LEDGER_BRANCH = "automation/compatibility-state"
LEDGER_PATH = "compatibility-state.json"
LEDGER_MARKER = "plex-compatibility-ledger-v1"


def validate_ledger(ledger):
    if (not isinstance(ledger, dict) or ledger.get("schema_version") != 1
            or not isinstance(ledger.get("attempts"), dict)
            or not isinstance(ledger.get("superseded"), list)):
        raise ValueError("Invalid compatibility ledger schema")
    keys = set(ledger["attempts"])
    if any(not re.fullmatch(r"[0-9a-f]{64}", key) for key in keys):
        raise ValueError("Invalid compatibility ledger fingerprint")
    for slot in ("active", "pending"):
        key = ledger.get(slot)
        if slot not in ledger or (key is not None and key not in keys):
            raise ValueError("Invalid compatibility ledger slot")
        if key is not None and ledger["attempts"][key].get("state") != slot:
            raise ValueError("Invalid compatibility ledger slot state")
    if ledger["active"] is not None and ledger["active"] == ledger["pending"]:
        raise ValueError("Duplicate compatibility ledger slots")
    if any(key not in keys for key in ledger["superseded"]):
        raise ValueError("Invalid superseded fingerprint")
    for attempt in ledger["attempts"].values():
        if (not isinstance(attempt, dict)
                or attempt.get("state") not in TERMINAL | {"active", "pending", "superseded"}
                or type(attempt.get("automatic_retries", 0)) is not int
                or not 0 <= attempt.get("automatic_retries", 0) <= 1):
            raise ValueError("Invalid compatibility attempt")
    return ledger


def api_missing(error):
    return "HTTP 404" in (error.stderr or "")


def ledger_state(repository):
    # Only absence permits migration. Authentication and malformed content fail
    # closed, and a Contents SHA binds each subsequent update to this read.
    path = f"repos/{repository}/contents/{LEDGER_PATH}?ref={quote(LEDGER_BRANCH, safe='')}"
    try:
        content = github_api(path)
    except subprocess.CalledProcessError as error:
        if not api_missing(error):
            raise
    else:
        if (content.get("type") != "file" or content.get("encoding") != "base64"
                or not SHA.fullmatch(content.get("sha", ""))):
            raise ValueError("Malformed compatibility state file")
        try:
            envelope = json.loads(base64.b64decode(content["content"].replace("\n", ""), validate=True))
        except (ValueError, KeyError) as error:
            raise ValueError("Malformed compatibility state content") from error
        if not isinstance(envelope, dict) or envelope.get("marker") != LEDGER_MARKER:
            raise ValueError("Compatibility state marker mismatch")
        return content["sha"], validate_ledger(envelope.get("ledger"))

    try:
        github_api(f"repos/{repository}/git/ref/heads/{LEDGER_BRANCH}")
    except subprocess.CalledProcessError as error:
        if not api_missing(error):
            raise
    else:
        raise ValueError("Existing compatibility state branch has no ledger file")

    # Read the existing administrative issue once during migration, including
    # closed issues. This path never creates or changes an issue.
    try:
        issue = github_api(f"repos/{repository}/issues/34")
    except subprocess.CalledProcessError as error:
        if not api_missing(error):
            raise
        return None, empty_ledger()
    if issue.get("title") != "Plex compatibility automation ledger" or issue.get("pull_request"):
        raise ValueError("Legacy compatibility ledger identity mismatch")
    body = issue.get("body") or ""
    prefix = "<!-- plex-compatibility-ledger-v1 -->\n```json\n"
    if not body.startswith(prefix) or not body.endswith("\n```\n"):
        raise ValueError("Legacy compatibility ledger has been edited or is malformed")
    return None, validate_ledger(json.loads(body[len(prefix):-5]))


def ledger_body(ledger):
    # Kept for validating the legacy issue migration format.
    return "<!-- plex-compatibility-ledger-v1 -->\n```json\n" + json.dumps(ledger, sort_keys=True) + "\n```\n"


def persist_ledger(repository, content_sha, ledger):
    validate_ledger(ledger)
    ledger = json.loads(json.dumps(ledger))
    for key, attempt in ledger["attempts"].items():
        if key not in (ledger.get("active"), ledger.get("pending")):
            attempt.pop("plan", None)
    body = json.dumps({"marker": LEDGER_MARKER, "ledger": ledger}, sort_keys=True) + "\n"
    if len(body.encode()) > 60000:
        raise ValueError("Attempt ledger requires archival before further work; no fingerprint was forgotten")
    default = github_api(f"repos/{repository}")["default_branch"]
    if default == LEDGER_BRANCH:
        raise ValueError("Compatibility state cannot use the default branch")
    ref_path = f"repos/{repository}/git/ref/heads/{LEDGER_BRANCH}"
    try:
        github_api(ref_path)
    except subprocess.CalledProcessError as error:
        if not api_missing(error):
            raise
        source = github_api(f"repos/{repository}/git/ref/heads/{default}")["object"]["sha"]
        try:
            github_api(f"repos/{repository}/git/refs", "POST", {
                "ref": "refs/heads/" + LEDGER_BRANCH, "sha": source})
        except subprocess.CalledProcessError:
            # Another initializer may have created the ref. Verify it exists;
            # the Contents create/update still rejects stale or missing SHAs.
            github_api(ref_path)
    payload = {"message": "Preserve compatibility attempt history", "branch": LEDGER_BRANCH,
               "content": base64.b64encode(body.encode()).decode()}
    if content_sha is not None:
        if not SHA.fullmatch(content_sha):
            raise ValueError("Invalid compatibility content SHA")
        payload["sha"] = content_sha
    # No retry on 409/422: reread and recompute the transition instead of
    # overwriting another coordinator's state.
    github_api(f"repos/{repository}/contents/{LEDGER_PATH}", "PUT", payload)


def claim(repository, plan, run_id, retry=False):
    content_sha, ledger = ledger_state(repository)
    active = ledger.get("active")
    if active:
        active_run = ledger["attempts"][active].get("run_id")
        if not active_run:
            raise ValueError("Active ledger entry has no owning workflow run")
        run = github_api(f"repos/{repository}/actions/runs/{active_run}")
        if run["status"] == "completed":
            # A killed coordinator cannot claim its downstream work passed.
            ledger = finish(ledger, active, "insufficient-evidence", run["html_url"])
    ledger, action = enqueue(ledger, plan, retry)
    if action == "start-probe":
        ledger["attempts"][plan["fingerprint"]]["run_id"] = str(run_id)
    persist_ledger(repository, content_sha, ledger)
    return action


def complete(repository, plan, run_id, result, run_url, category="insufficient-evidence"):
    content_sha, ledger = ledger_state(repository)
    key = plan["fingerprint"]
    attempt = ledger["attempts"].get(key, {})
    if attempt.get("run_id") != str(run_id):
        raise ValueError("Completion run does not own the active attempt")
    # A successful probe is still insufficient release evidence. Generic
    # workflow failure is not automatically called a compatibility regression.
    if result not in {"success", "failure", "cancelled", "skipped"}:
        raise ValueError("Unknown workflow conclusion")
    if category == "probe-passed" and result != "success":
        raise ValueError("Failed workflow cannot supply a passing observation")
    ledger = finish(ledger, key, category, run_url)
    persist_ledger(repository, content_sha, ledger)
    return category


def dispatch_followup(repository, plan, category, run_url):
    default = github_api(f"repos/{repository}")["default_branch"]
    current = github_api(f"repos/{repository}/git/ref/heads/{default}")["object"]["sha"]
    if current != plan["source_sha"]:
        raise ValueError("Default source changed; restage or reobserve instead of dispatching stale work")
    if category == "probe-passed":
        latest = plan["profiles"][0]
        inputs = {"merged_source_sha": plan["source_sha"], "plex_version": latest["plex_version"],
                  "upstream_profile_json": json.dumps(latest, separators=(",", ":")), "profile_id": latest["id"]}
        workflow = "stage-compatibility-release.yml"
    elif category == "regression":
        inputs = {"fix_request": json.dumps(fix_request(plan, category, run_url), separators=(",", ":"))}
        workflow = "compatibility-fix.yml"
    else:
        return "no-followup"
    github_api(f"repos/{repository}/actions/workflows/{workflow}/dispatches", "POST",
               {"ref": default, "inputs": inputs})
    return workflow


def report_pr(repository, plan, category, run_url):
    body = report(plan, category, run_url)
    marker = "plex-compatibility-report:" + plan["fingerprint"]
    prs = github_api(f"repos/{repository}/pulls?state=all&per_page=100")
    matching = [pr for pr in prs if marker in (pr.get("body") or "")]
    if matching:
        return matching[0]["html_url"]
    base = github_api(f"repos/{repository}")["default_branch"]
    base_commit = github_api(f"repos/{repository}/git/ref/heads/{base}")["object"]["sha"]
    if base_commit != plan["source_sha"]:
        raise ValueError("Default branch changed; report must be regenerated against the current source")
    tree_sha = github_api(f"repos/{repository}/git/commits/{base_commit}")["tree"]["sha"]
    entries = [{"path": f"docs/compatibility-observations/{plan['fingerprint']}.md",
                "mode": "100644", "type": "blob", "content": body}]
    if category == "regression":
        import base64
        changelog = github_api(f"repos/{repository}/contents/CHANGELOG.md?ref={base_commit}")
        original = base64.b64decode(changelog["content"]).decode()
        entry = ("## Observed upstream compatibility regression\n\n"
                 f"Native testing of Plex {plan['profiles'][0]['plex_version']} failed compatibility assertions. "
                 f"Root cause remains under investigation. [Evidence]({run_url}).\n\n")
        entries.append({"path": "CHANGELOG.md", "mode": "100644", "type": "blob", "content": entry + original})
    tree = github_api(f"repos/{repository}/git/trees", "POST", {"base_tree": tree_sha, "tree": entries})
    commit = github_api(f"repos/{repository}/git/commits", "POST", {
        "message": "Preserve measured Plex compatibility observations\n\nConfidence: high\nScope-risk: narrow\nTested: deterministic evidence classification",
        "tree": tree["sha"], "parents": [base_commit]})
    branch = "automation/plex-report-" + plan["fingerprint"]
    github_api(f"repos/{repository}/git/refs", "POST", {"ref": "refs/heads/" + branch, "sha": commit["sha"]})
    pr = github_api(f"repos/{repository}/pulls", "POST", {
        "title": f"Record Plex compatibility: {category}", "head": branch, "base": base,
        "body": body + f"\n<!-- {marker} -->\n", "draft": False})
    return pr["html_url"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    plan_cmd = sub.add_parser("plan")
    plan_cmd.add_argument("--snapshot", required=True)
    plan_cmd.add_argument("--policy", default=".github/plex-compatibility-policy.json")
    plan_cmd.add_argument("--history", default=".github/plex-compatibility-profiles.json")
    plan_cmd.add_argument("--source-sha", required=True)
    plan_cmd.add_argument("--output", required=True)
    report_cmd = sub.add_parser("report")
    report_cmd.add_argument("--plan", required=True)
    report_cmd.add_argument("--category", required=True)
    report_cmd.add_argument("--run-url")
    report_cmd.add_argument("--output", required=True)
    validate_cmd = sub.add_parser("validate-profile")
    validate_cmd.add_argument("--input", required=True)
    validate_cmd.add_argument("--output", required=True)
    claim_cmd = sub.add_parser("claim")
    complete_cmd = sub.add_parser("complete")
    pr_cmd = sub.add_parser("report-pr")
    observation_cmd = sub.add_parser("observe")
    followup_cmd = sub.add_parser("dispatch-followup")
    for command in (claim_cmd, complete_cmd, pr_cmd):
        command.add_argument("--plan", required=True)
        command.add_argument("--repository", required=True)
    claim_cmd.add_argument("--run-id", required=True)
    claim_cmd.add_argument("--retry", action="store_true")
    complete_cmd.add_argument("--run-id", required=True)
    complete_cmd.add_argument("--result", required=True)
    complete_cmd.add_argument("--run-url", required=True)
    pr_cmd.add_argument("--category", required=True)
    pr_cmd.add_argument("--run-url", required=True)
    observation_cmd.add_argument("--plan", required=True)
    observation_cmd.add_argument("--observation")
    observation_cmd.add_argument("--observations-root")
    observation_cmd.add_argument("--run-id", required=True)
    observation_cmd.add_argument("--run-attempt", required=True, type=int)
    complete_cmd.add_argument("--category", default="insufficient-evidence")
    followup_cmd.add_argument("--plan", required=True)
    followup_cmd.add_argument("--repository", required=True)
    followup_cmd.add_argument("--category", required=True)
    followup_cmd.add_argument("--run-url", required=True)
    args = parser.parse_args()
    if args.command == "plan":
        policy = read_json(args.policy)
        fixture_hash = hashlib.sha256(Path(policy["fixtureManifest"]).read_bytes()).hexdigest()
        plan = discovery_plan(read_json(args.snapshot), policy, read_json(args.history),
                              args.source_sha, fixture_hash)
        write_json(args.output, plan)
        print(json.dumps(plan, sort_keys=True))
    elif args.command == "report":
        Path(args.output).write_text(report(read_json(args.plan), args.category, args.run_url))
    elif args.command == "validate-profile":
        emit_profile(read_json(args.input), args.output)
    elif args.command == "claim":
        action = claim(args.repository, read_json(args.plan), args.run_id, args.retry)
        print(action)
        if os.environ.get("GITHUB_OUTPUT"):
            with open(os.environ["GITHUB_OUTPUT"], "a") as stream:
                stream.write(f"action={action}\n")
    elif args.command == "complete":
        print(complete(args.repository, read_json(args.plan), args.run_id, args.result, args.run_url, args.category))
    elif args.command == "observe":
        if args.observations_root:
            if args.observation:
                raise ValueError("Use either one observation or the selected-profile artifact root")
            print(classify_selected_observations(read_json(args.plan), args.observations_root, args.run_id, args.run_attempt))
        else:
            observation = read_json(args.observation) if args.observation and Path(args.observation).is_file() else None
            print(classify_observation(read_json(args.plan), observation, args.run_id, args.run_attempt))
    elif args.command == "dispatch-followup":
        print(dispatch_followup(args.repository, read_json(args.plan), args.category, args.run_url))
    else:
        print(report_pr(args.repository, read_json(args.plan), args.category, args.run_url))


if __name__ == "__main__":
    main()

"""Validate candidate provenance and security state without changing GitHub data."""

import argparse
import json
import os
from pathlib import Path
import re
import sys
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode
from urllib.request import Request, urlopen


class GateError(Exception):
    """A release condition was not positively verified."""


def validate_request(commit, ref, candidate_run, verified_hash):
    if ref != "refs/heads/main":
        raise GateError("Release workflow must run on main.")
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise GateError("Expected an exact source commit SHA.")
    if not re.fullmatch(r"[1-9][0-9]*", candidate_run):
        raise GateError("A verified candidate workflow run ID is required.")
    if not re.fullmatch(r"[0-9a-fA-F]{64}", verified_hash):
        raise GateError("The SHA-256 of the executable tested on Windows 11 is required.")


def validate_candidate(version, info, commit, verified_hash):
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise GateError("Expected a stable numeric Cargo version.")
    if not isinstance(info, dict) or (
        info.get("version") != version or info.get("source_commit") != commit
    ):
        raise GateError("Candidate was not built from this version and commit.")
    candidate_hash = info.get("executable_sha256")
    if not isinstance(candidate_hash, str) or candidate_hash.lower() != verified_hash.lower():
        raise GateError("Candidate executable differs from the Windows 11 verified executable.")


def github_api(repository, token):
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise GateError("Expected an owner/repository name.")

    def get(path):
        request = Request(
            f"https://api.github.com/repos/{repository}/{path}",
            headers={
                "Authorization": f"Bearer {token}",
                "User-Agent": "TaskbarMonitor-ReleaseGate",
                "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2026-03-10",
            },
        )
        with urlopen(request, timeout=30) as response:
            return json.load(response)

    return get


def require_candidate_origin(api, repository, commit, candidate_run):
    run = api(f"actions/runs/{candidate_run}")
    if not isinstance(run, dict) or (
        run.get("id") != int(candidate_run)
        or run.get("repository", {}).get("full_name") != repository
        or run.get("head_repository", {}).get("full_name") != repository
        or run.get("head_branch") != "main"
        or run.get("head_sha") != commit
        or run.get("path") != ".github/workflows/release.yml"
        or run.get("event") != "workflow_dispatch"
        or run.get("status") != "completed"
        or run.get("conclusion") != "success"
    ):
        raise GateError("Candidate must come from a completed successful Release run on this repository's exact main commit.")


def require_current_main(api, commit):
    response = api("git/ref/heads/main")
    reference = response.get("object", {}) if isinstance(response, dict) else {}
    if reference.get("type") != "commit" or reference.get("sha") != commit:
        raise GateError("Main advanced or could not be verified; rerun on current main.")


def require_unused_tag(api, version):
    try:
        api(f"git/ref/tags/v{version}")
    except HTTPError as error:
        # Only an explicitly missing reference permits a new release.
        if error.code == 404:
            return
        raise
    raise GateError(f"Tag v{version} already exists; never replace or reuse a release tag.")


def require_clean_analysis(api, commit):
    parameters = {"ref": "refs/heads/main", "tool_name": "CodeQL", "per_page": 100}
    analyses = api("code-scanning/analyses?" + urlencode(parameters))
    if not isinstance(analyses, list):
        raise GateError("CodeQL analysis response was not a list.")
    # GitHub lists newest analyses first. An earlier success must not hide the
    # newest Rust scan's failure or a mismatch with the release commit.
    rust_analyses = [
        analysis
        for analysis in analyses
        if isinstance(analysis, dict)
        and analysis.get("ref") == "refs/heads/main"
        and analysis.get("tool", {}).get("name") == "CodeQL"
        and analysis.get("category", "").rstrip("/") == "/language:rust"
    ]
    latest = rust_analyses[0] if rust_analyses else {}
    if (
        latest.get("commit_sha") != commit
        or latest.get("error") != ""
        or not isinstance(latest.get("rules_count"), int)
        or latest["rules_count"] <= 0
    ):
        raise GateError("No complete latest CodeQL Rust analysis exists for this exact commit.")
    for severity in ("high", "critical"):
        query = {**parameters, "state": "open", "severity": severity, "per_page": 1}
        alerts = api("code-scanning/alerts?" + urlencode(query))
        if not isinstance(alerts, list):
            raise GateError("CodeQL alert response was not a list.")
        if alerts:
            raise GateError(f"Open {severity} CodeQL security findings block release.")


def preflight(api, repository, version, info, commit, ref, candidate_run, verified_hash):
    validate_request(commit, ref, candidate_run, verified_hash)
    validate_candidate(version, info, commit, verified_hash)
    require_unused_tag(api, version)
    require_candidate_origin(api, repository, commit, candidate_run)
    require_current_main(api, commit)
    require_clean_analysis(api, commit)
    require_current_main(api, commit)


def verify_tag(api, version, info, commit, verified_hash):
    validate_candidate(version, info, commit, verified_hash)
    require_current_main(api, commit)
    response = api(f"git/ref/tags/v{version}")
    reference = response.get("object", {}) if isinstance(response, dict) else {}
    # The workflow creates a new lightweight tag atomically through Git refs.
    # A changed tag type or target must stop publication.
    if reference.get("type") != "commit" or reference.get("sha") != commit:
        raise GateError("Release tag does not point directly to the verified source commit.")
    require_current_main(api, commit)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--verify-origin", action="store_true", help="check the source run before downloading artifacts")
    mode.add_argument("--verify-tag", action="store_true", help="check the newly created tag before publishing")
    arguments = parser.parse_args(argv)
    try:
        repository = os.environ["GITHUB_REPOSITORY"]
        commit = os.environ["GITHUB_SHA"]
        ref = os.environ["GITHUB_REF"]
        candidate_run = os.environ.get("VERIFIED_CANDIDATE_RUN_ID", "")
        verified_hash = os.environ.get("VERIFIED_EXECUTABLE_SHA256", "")
        validate_request(commit, ref, candidate_run, verified_hash)
        api = github_api(repository, os.environ["GH_TOKEN"])
        if arguments.verify_origin:
            require_candidate_origin(api, repository, commit, candidate_run)
            print(f"Verified candidate origin: run {candidate_run} at {commit}.")
            return 0
        cargo = Path("Cargo.toml").read_text(encoding="utf-8")
        match = re.search(r'^version = "([0-9]+\.[0-9]+\.[0-9]+)"', cargo, re.M)
        if not match:
            raise GateError("Expected a stable numeric Cargo version.")
        version = match.group(1)
        info = json.loads(Path("dist/build-info.json").read_text(encoding="utf-8-sig"))
        if arguments.verify_tag:
            verify_tag(api, version, info, commit, verified_hash)
        else:
            preflight(api, repository, version, info, commit, ref, candidate_run, verified_hash)
        print(f"Release {'tag verification' if arguments.verify_tag else 'gate'} passed for {version} at {commit}.")
        return 0
    except (GateError, HTTPError, URLError, OSError, KeyError, ValueError, TypeError) as error:
        print(f"Release blocked: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

"""Release policy tests use local fixtures only; no GitHub requests or writes."""

import copy
import importlib.util
from pathlib import Path
import unittest
from urllib.error import HTTPError, URLError
from urllib.parse import parse_qs, urlsplit


spec = importlib.util.spec_from_file_location("release_gate", Path(__file__).with_name("release-gate.py"))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)

REPOSITORY = "DARC0625/taskbar-monitor"
COMMIT = "a" * 40
OTHER_COMMIT = "b" * 40
VERSION = "0.5.0"
RUN_ID = "12345"
EXE_HASH = "c" * 64
INFO = {"version": VERSION, "source_commit": COMMIT, "executable_sha256": EXE_HASH}
ANALYSIS = {
    "ref": "refs/heads/main", "tool": {"name": "CodeQL"},
    "category": "/language:rust", "commit_sha": COMMIT,
    "error": "", "rules_count": 20,
}
RUN = {
    "id": int(RUN_ID), "repository": {"full_name": REPOSITORY},
    "head_repository": {"full_name": REPOSITORY},
    "head_branch": "main", "head_sha": COMMIT,
    "path": ".github/workflows/release.yml", "event": "workflow_dispatch",
    "status": "completed", "conclusion": "success",
}


def reference(commit=COMMIT, kind="commit"):
    return {"object": {"type": kind, "sha": commit}}


class FixtureApi:
    def __init__(self):
        self.main = [reference(), reference()]
        self.tag = HTTPError("fixture://tag", 404, "Not found", {}, None)
        self.analyses = [copy.deepcopy(ANALYSIS)]
        self.run = copy.deepcopy(RUN)
        self.alerts = {"high": [], "critical": []}
        self.calls = []

    def __call__(self, path):
        self.calls.append(path)
        if path == "git/ref/heads/main":
            result = self.main.pop(0)
        elif path == f"git/ref/tags/v{VERSION}":
            result = self.tag
        elif path == f"actions/runs/{RUN_ID}":
            result = self.run
        elif path.startswith("code-scanning/analyses?"):
            result = self.analyses
        elif path.startswith("code-scanning/alerts?"):
            query = parse_qs(urlsplit(path).query)
            assert query["ref"] == ["refs/heads/main"]
            assert query["tool_name"] == ["CodeQL"]
            assert query["state"] == ["open"]
            assert query["per_page"] == ["1"]
            result = self.alerts[query["severity"][0]]
        else:
            raise AssertionError(f"Unexpected API request: {path}")
        if isinstance(result, Exception):
            raise result
        return result


class ReleaseGateTests(unittest.TestCase):
    def setUp(self):
        self.api = FixtureApi()

    def preflight(self, info=None, ref="refs/heads/main", run_id=RUN_ID, verified_hash=EXE_HASH):
        gate.preflight(self.api, REPOSITORY, VERSION, INFO if info is None else info,
                       COMMIT, ref, run_id, verified_hash)

    def test_matching_candidate_clean_scan_and_unused_tag_pass(self):
        self.preflight()
        self.assertEqual(self.api.calls.count("git/ref/heads/main"), 2)

    def test_wrong_candidate_commit_is_rejected_before_api_access(self):
        with self.assertRaisesRegex(gate.GateError, "Candidate"):
            self.preflight({**INFO, "source_commit": OTHER_COMMIT})
        self.assertEqual(self.api.calls, [])

    def test_wrong_candidate_version_is_rejected(self):
        with self.assertRaisesRegex(gate.GateError, "Candidate"):
            self.preflight({**INFO, "version": "0.4.0"})

    def test_executable_must_match_the_manually_verified_hash(self):
        with self.assertRaisesRegex(gate.GateError, "Windows 11 verified"):
            self.preflight({**INFO, "executable_sha256": "d" * 64})

    def test_hash_comparison_accepts_uppercase_sha256(self):
        self.preflight(verified_hash=EXE_HASH.upper())

    def test_missing_invalid_run_id_and_hash_are_rejected(self):
        for run_id, verified_hash in (("", EXE_HASH), ("../1", EXE_HASH), (RUN_ID, ""), (RUN_ID, "abc")):
            with self.subTest(run_id=run_id, verified_hash=verified_hash):
                with self.assertRaises(gate.GateError):
                    self.preflight(run_id=run_id, verified_hash=verified_hash)
        self.assertEqual(self.api.calls, [])

    def test_branch_dispatch_cannot_publish(self):
        with self.assertRaisesRegex(gate.GateError, "main"):
            self.preflight(ref="refs/heads/contributor")

    def test_untrusted_or_wrong_candidate_run_is_rejected(self):
        changes = (
            {"id": int(RUN_ID) + 1},
            {"repository": {"full_name": "elsewhere/repository"}},
            {"head_repository": {"full_name": "contributor/fork"}},
            {"head_branch": "contributor"}, {"head_sha": OTHER_COMMIT},
            {"path": ".github/workflows/ci.yml"}, {"event": "pull_request"},
            {"status": "in_progress"}, {"conclusion": "failure"},
        )
        for change in changes:
            with self.subTest(change=change):
                self.api = FixtureApi()
                self.api.run.update(change)
                with self.assertRaisesRegex(gate.GateError, "Candidate must come"):
                    self.preflight()

    def test_existing_tag_is_rejected_even_if_it_has_the_right_commit(self):
        for commit in (COMMIT, OTHER_COMMIT):
            with self.subTest(commit=commit):
                self.api.tag = reference(commit)
                with self.assertRaisesRegex(gate.GateError, "already exists"):
                    self.preflight()

    def test_unreadable_tag_is_not_treated_as_a_missing_tag(self):
        for status in (401, 403, 429, 500):
            with self.subTest(status=status):
                self.api.tag = HTTPError("fixture://tag", status, "Unavailable", {}, None)
                with self.assertRaises(HTTPError):
                    self.preflight()

    def test_network_failure_blocks_release(self):
        self.api.tag = URLError("Network unavailable")
        with self.assertRaises(URLError):
            self.preflight()

    def test_main_advanced_before_the_scan_is_rejected(self):
        self.api.main[0] = reference(OTHER_COMMIT)
        with self.assertRaisesRegex(gate.GateError, "Main advanced"):
            self.preflight()

    def test_main_advanced_during_alert_lookup_is_rejected(self):
        self.api.main[-1] = reference(OTHER_COMMIT)
        with self.assertRaisesRegex(gate.GateError, "Main advanced"):
            self.preflight()

    def test_missing_analysis_is_rejected(self):
        self.api.analyses = []
        with self.assertRaisesRegex(gate.GateError, "CodeQL Rust analysis"):
            self.preflight()

    def test_failed_wrong_commit_wrong_language_and_empty_scan_are_rejected(self):
        changes = (
            {"error": "extraction failed"}, {"commit_sha": OTHER_COMMIT},
            {"category": "/language:javascript"}, {"ref": "refs/heads/contributor"},
            {"tool": {"name": "Another scanner"}}, {"rules_count": 0},
        )
        for change in changes:
            with self.subTest(change=change):
                self.api = FixtureApi()
                self.api.analyses = [{**ANALYSIS, **change}]
                with self.assertRaisesRegex(gate.GateError, "CodeQL Rust analysis"):
                    self.preflight()

    def test_older_success_does_not_override_a_newer_failed_scan(self):
        self.api.analyses = [{**ANALYSIS, "error": "new failure"}, copy.deepcopy(ANALYSIS)]
        with self.assertRaisesRegex(gate.GateError, "CodeQL Rust analysis"):
            self.preflight()

    def test_open_high_and_critical_alerts_each_block_release(self):
        for severity in ("high", "critical"):
            with self.subTest(severity=severity):
                self.api = FixtureApi()
                self.api.alerts[severity] = [{"number": 7}]
                with self.assertRaisesRegex(gate.GateError, f"Open {severity}"):
                    self.preflight()

    def test_malformed_empty_alert_response_does_not_pass(self):
        self.api.alerts["high"] = {}
        with self.assertRaisesRegex(gate.GateError, "not a list"):
            self.preflight()

    def test_new_tag_points_to_the_verified_commit(self):
        self.api.tag = reference()
        gate.verify_tag(self.api, VERSION, INFO, COMMIT, EXE_HASH)
        self.assertEqual(self.api.calls.count("git/ref/heads/main"), 2)

    def test_changed_tag_target_or_type_blocks_publication(self):
        for tag in (reference(OTHER_COMMIT), reference(kind="tag")):
            with self.subTest(tag=tag):
                self.api = FixtureApi()
                self.api.tag = tag
                with self.assertRaisesRegex(gate.GateError, "Release tag"):
                    gate.verify_tag(self.api, VERSION, INFO, COMMIT, EXE_HASH)

    def test_deleted_tag_blocks_publication(self):
        with self.assertRaises(HTTPError):
            gate.verify_tag(self.api, VERSION, INFO, COMMIT, EXE_HASH)


if __name__ == "__main__":
    unittest.main()

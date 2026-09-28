import importlib.util
from pathlib import Path
import unittest


SCRIPT = Path(__file__).with_name("select-app-server-build.py")
SPEC = importlib.util.spec_from_file_location("select_app_server_build", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)

REPOSITORY = "circuitandchisel/unbiased-app-server"
COMMIT = "a" * 40


def valid_run(**overrides):
    run = {
        "id": 123,
        "repository": {"full_name": REPOSITORY},
        "head_sha": COMMIT,
        "head_branch": "main",
        "event": "push",
        "status": "completed",
        "conclusion": "success",
    }
    run.update(overrides)
    return run


def valid_artifacts(**overrides):
    artifacts = [
        {
            "name": name,
            "expired": False,
            "size_in_bytes": 100,
            "digest": "sha256:" + "b" * 64,
            "workflow_run": {"id": 123},
        }
        for name in MODULE.ARTIFACT_NAMES
    ]
    artifacts[0].update(overrides)
    return {"artifacts": artifacts}


class SelectAppServerBuildTests(unittest.TestCase):
    def test_selects_matching_complete_main_build(self):
        selected = MODULE.select_run(
            REPOSITORY, COMMIT, {"workflow_runs": [valid_run()]}, lambda _: valid_artifacts()
        )
        self.assertEqual(selected, 123)

    def test_rejects_untrusted_or_failed_runs(self):
        for run in (
            valid_run(event="pull_request"),
            valid_run(head_sha="b" * 40),
            valid_run(head_branch="release"),
            valid_run(repository={"full_name": "someone/else"}),
            valid_run(conclusion="failure"),
        ):
            with self.subTest(run=run), self.assertRaisesRegex(ValueError, "No successful main build"):
                MODULE.select_run(REPOSITORY, COMMIT, {"workflow_runs": [run]}, lambda _: valid_artifacts())

    def test_rejects_incomplete_or_expired_artifacts(self):
        for artifacts in (
            {"artifacts": valid_artifacts()["artifacts"][:-1]},
            valid_artifacts(expired=True),
            valid_artifacts(size_in_bytes=0),
            valid_artifacts(digest=""),
            valid_artifacts(workflow_run={"id": 456}),
        ):
            with self.subTest(artifacts=artifacts), self.assertRaisesRegex(ValueError, "No successful main build"):
                MODULE.select_run(REPOSITORY, COMMIT, {"workflow_runs": [valid_run()]}, lambda _: artifacts)

    def test_falls_back_to_another_successful_run(self):
        runs = {"workflow_runs": [valid_run(id=456), valid_run()]}
        selected = MODULE.select_run(
            REPOSITORY,
            COMMIT,
            runs,
            lambda run_id: {"artifacts": []} if run_id == 456 else valid_artifacts(),
        )
        self.assertEqual(selected, 123)


if __name__ == "__main__":
    unittest.main()

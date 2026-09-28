#!/usr/bin/env python3
"""Select a complete main-branch build for an app-server release tag."""

import json
import os
import re
import sys
from urllib.parse import urlencode
from urllib.request import Request, urlopen


TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-musl",
    "x86_64-unknown-linux-musl",
)
ARTIFACT_NAMES = {f"app-server-{target}" for target in TARGETS}


def github_get(path, token):
    base_url = os.environ.get("GITHUB_API_URL", "https://api.github.com")
    request = Request(
        f"{base_url.rstrip('/')}/{path.lstrip('/')}",
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "User-Agent": "unbiased-app-server-release",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    with urlopen(request, timeout=30) as response:
        return json.load(response)


def select_run(repository, commit, runs, get_artifacts):
    for run in runs.get("workflow_runs", []):
        if not (
            run.get("repository", {}).get("full_name") == repository
            and run.get("head_sha") == commit
            and run.get("head_branch") == "main"
            and run.get("event") == "push"
            and run.get("status") == "completed"
            and run.get("conclusion") == "success"
        ):
            continue

        run_id = run["id"]
        artifacts = get_artifacts(run_id).get("artifacts", [])
        if len(artifacts) != len(ARTIFACT_NAMES):
            continue
        if {artifact.get("name") for artifact in artifacts} != ARTIFACT_NAMES:
            continue
        if any(
            artifact.get("expired")
            or artifact.get("size_in_bytes", 0) <= 0
            or not re.fullmatch(r"sha256:[0-9a-f]{64}", artifact.get("digest", ""))
            or artifact.get("workflow_run", {}).get("id") != run_id
            for artifact in artifacts
        ):
            continue
        return run_id

    raise ValueError(
        "No successful main build with all four unexpired artifacts matches this tag. "
        "Wait for the main build to finish, then rerun this release workflow."
    )


def main():
    if len(sys.argv) != 3:
        raise SystemExit("usage: select-app-server-build.py OWNER/REPO COMMIT_SHA")
    repository, commit = sys.argv[1:]
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise SystemExit("invalid repository")
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise SystemExit("invalid commit SHA")
    token = os.environ.get("GH_TOKEN")
    if not token:
        raise SystemExit("GH_TOKEN is required")

    query = urlencode(
        {"branch": "main", "event": "push", "head_sha": commit, "status": "success", "per_page": 100}
    )
    runs = github_get(
        f"repos/{repository}/actions/workflows/unbiased-app-server-release.yml/runs?{query}",
        token,
    )
    try:
        run_id = select_run(
            repository,
            commit,
            runs,
            lambda candidate: github_get(f"repos/{repository}/actions/runs/{candidate}/artifacts", token),
        )
    except ValueError as error:
        raise SystemExit(str(error)) from error
    print(run_id)


if __name__ == "__main__":
    main()

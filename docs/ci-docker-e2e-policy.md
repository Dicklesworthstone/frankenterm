# CI Docker E2E Policy

This document records the `wa-nu4.3.9.28` decision for Docker-dependent
end-to-end coverage.

## Decision

Run real Docker-dependent E2E only on GitHub-hosted Linux runners. On
GitHub-hosted macOS, do not install or start Docker Desktop in CI. Instead, run
the mocked setup-remote helper E2E and publish a waiver artifact that explains
why the real Docker scenario is intentionally skipped.

## Current Docker-Dependent Tests

| Test surface | Docker dependency | CI policy |
| --- | --- | --- |
| `scripts/e2e_test.sh setup_remote_docker` | Real Docker engine, `docker build`, two sshd containers, SSH, `jq` | Run on `ubuntu-latest` in `.github/workflows/ci.yml` `docker-e2e` with `FT_E2E_ENABLE_SETUP_REMOTE=1`; upload `e2e-artifacts/` and driver log. |
| `tests/e2e/test_ft_nu4_3_3_11.sh` | Mocked `docker`, `ssh`, `ssh-keygen`, and `ft`; no real Docker daemon | Run on `macos-14` in `.github/workflows/ci.yml` `docker-e2e` as the macOS replacement signal; upload mock logs/artifacts. |
| `tests/e2e/distributed_compose_smoke.sh` | Real Docker Compose, Linux container topology, Linux `ft` binary | Keep opt-in through `FT_E2E_DISTRIBUTED_COMPOSE_BINARY`; do not run on hosted macOS unless a future self-hosted runner supplies a supported Docker engine. |

## Rationale

The real `setup_remote_docker` scenario validates Linux remote setup by building
a Debian sshd image, starting good and failure-injection containers, applying
`ft setup remote`, checking idempotency, and preserving rollback artifacts. That
is a Linux-container scenario, and Ubuntu hosted runners include Docker Client,
Docker Server, Docker Buildx, and Docker Compose.

GitHub-hosted macOS runner images are maintained as macOS VMs and the published
macOS 14 image software list does not include Docker. GitHub's hosted-runner
documentation also warns that nested virtualization on hosted runners is
experimental and unsupported. Installing Docker Desktop inside every macOS CI
run would therefore make the ship gate slower and less trustworthy than the
behavior it is trying to prove.

The macOS replacement signal is split deliberately:

- `cargo-install-validation.yml` remains the macOS install-path E2E for the `ft`
  binary.
- `operator-shell-tests` continues to cover macOS shell portability.
- `docker-e2e` runs `tests/e2e/test_ft_nu4_3_3_11.sh` on macOS so helper
  semantics, rollback artifacts, and summary JSON stay exercised without a real
  Docker daemon.

## Revisit Conditions

Reconsider this waiver only if one of these becomes true:

- GitHub-hosted macOS images ship and support a Docker engine by default.
- The project adds a self-hosted macOS runner with a maintained Docker Desktop
  installation and enough CPU/memory for Linux containers.
- The Docker-dependent E2E changes from Linux-container validation to a
  macOS-specific remote-setup behavior that cannot be covered by the existing
  install and mocked-helper lanes.

Any future real macOS Docker lane must upload failure artifacts with
`actions/upload-artifact` and must keep Linux and macOS results distinguishable
in artifact names.

## Sources Checked

- GitHub-hosted runner documentation, checked 2026-04-28:
  <https://docs.github.com/en/actions/concepts/runners/github-hosted-runners>
- GitHub runner image software lists, checked 2026-04-28:
  <https://github.com/actions/runner-images>

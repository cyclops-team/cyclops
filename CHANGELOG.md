# Changelog

All notable user-facing changes are recorded here.

## Unreleased

### Added

- Generic role labels and a configuration-driven same-session message ACL.
- `commPact-setup` for one-command setup without hand-writing `team.conf`.
- `install.sh` for local installation followed by generic session bootstrap.
- Config-only generation for safe adoption of an existing tmux session.
- Automated regression coverage and GitHub Actions CI.
- `cyclops`, a unified CLI entry point over the commPact toolkit: bare `cyclops` bootstraps or attaches to a workspace, and `cyclops send|list|read|resolve|type|keys|name|adopt|layout|update|uninstall|...` forward to the matching commPact command.
- `frontend/static/install.sh`, the public `curl -fsSL https://usecyclops.dev/install.sh | sh` bootstrap that fetches the release from GitHub, runs it through the existing network-free `commPact-install`, and links `cyclops` onto `PATH`.

### Changed

- Package documentation is now repository-neutral and ready for public release.

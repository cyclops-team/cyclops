# Releasing commPact

This document prepares a release without selecting a project owner, license,
repository, visibility, or version. Those are release-owner decisions.

## Before the first public release

1. Decide whether the repository is public or private and create it under the
   approved owner or organization.
2. Choose the commPact project copyright holder and license. Keep the upstream
   smux attribution and MIT text required by its license. Update `LICENSE` and
   `NOTICE` together after that decision.
3. Replace the candidate version in `bin/commPact-install` with the approved
   release version.
4. Replace `<repo-url>` in `README.md` with the canonical clone URL.
5. Run the release checks from the repository root:

   ```sh
   bash -n bin/* lib/*.sh tests/*.sh install.sh
   bash tests/regression.sh
   ```

6. Confirm a fresh installation contains only `config/team.conf.example`, not
   a generated `config/team.conf` or local files such as `.DS_Store`.
7. Confirm the GitHub Actions `Test` workflow is green on Linux and macOS.

## Create the repository

Once the release owner has approved the decisions above, initialize and publish
the source intentionally:

```sh
git init
git add .
git status
git commit -m "Initial commPact release"
git branch -M main
git remote add origin <repo-url>
git push -u origin main
```

Review `git status` before the commit. `.gitignore` excludes generated team
configuration and common local artifacts, but it is still the release owner's
responsibility to check for credentials and unintended files.

## Tag a release

After CI passes and the release owner approves publication:

```sh
git tag -a vX.Y.Z -m "commPact vX.Y.Z"
git push origin vX.Y.Z
```

Create a GitHub release from that tag. Include the version, supported platforms,
the quick-start command from `README.md`, and any upgrade notes from
`CHANGELOG.md`.

## Upgrade and rollback

Users update an existing local installation with:

```sh
~/.commPact/bin/commPact-install update
```

The update keeps a timestamped backup and preserves a valid generated team
configuration. If a user needs a prior release, they can install that release
tree or restore an explicit backup with `commPact-install uninstall --restore PATH`.

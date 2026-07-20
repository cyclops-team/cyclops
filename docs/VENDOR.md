# Vendor and attribution

The release preserves the upstream MIT attribution material in `NOTICE`. The command layer is a local commPact adaptation with renamed metadata, package-relative paths, and isolated configuration parsing.

The default local home is `~/.commPact`; the installer never writes outside the selected destination.

The staged source is the review candidate. Its file hashes are evidence for this review, not a public release signature. The final commPact project license and distribution status are pending.

The installed `~/.smux` tree is not modified by this package. It remains the vendor and rollback reference until an operator-approved cutover changes that boundary.

## Verified provenance

- Upstream: `github.com/ShawnPana/smux`, MIT.
- Baseline: tmux-bridge v2.1.0 at `~/.smux/bin/tmux-bridge`.
- Baseline SHA-256: `c40a655e62a6ddc215622e5a227d702ea2fce5d5b09573cbeedbca1ecce29ced`.
- Staged candidate: `bin/commPact` SHA-256 `5a40adf69fa234c894ff5f176ba6dab2ed457eda7cd94007c113621124cc87b9`.

Verified local changes:

1. Config-driven session ACL and roster replace hardcoded allow labels.
2. Full commPact identity, metadata, and temp namespace rename ships with no alias.
3. Package-relative sibling commands avoid PATH lookup.
4. Leading-`@` target support resolves labels and preserves native targets.
5. Optional two-role weighted rightmost split directive.
6. SHA fallback chain uses shasum, sha256sum, then openssl.
7. Config-driven wrapper layer adds install, init, adopt, layout, msg, notice, and watchdog.
8. Doctor and hash subcommands are new.

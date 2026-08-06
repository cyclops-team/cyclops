# Cyclops website

The SvelteKit landing page for [usecyclops.dev](https://www.usecyclops.dev).
It is a sibling of the Rust source tree and is not part of the Cargo
workspace.

## Develop

```bash
npm ci
npm run dev
```

The development server uses port 5173 by default. Before committing website
changes, run both checks CI uses:

```bash
npm run check
npm run build
```

## Installer

`static/install.sh` is published as `https://www.usecyclops.dev/install.sh`.
It must remain byte-for-byte identical to the repository installer:

```bash
cmp ../scripts/install.sh static/install.sh
```

The shared script builds the current Rust implementation from `main` when it
is piped from the website, or builds the checked-out tree when run from a
clone. `tests/e2e/parity-check.sh` and the website CI job reject drift between
the two copies.

The public documentation source lives in `../docs/public/`; repository and
developer documentation starts at `../README.md` and
`../docs/development/HANDOFF.md`.

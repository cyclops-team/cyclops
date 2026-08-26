export const REPO_OWNER = 'cyclops-team';
export const REPO_NAME = 'cyclops';
export const REPO_URL = `https://github.com/${REPO_OWNER}/${REPO_NAME}`;
export const DOCS_URL = 'https://www.usecyclops.dev/docs';
export const LICENSE_URL = `${REPO_URL}/blob/main/LICENSE`;
export const GITHUB_API_URL = `https://api.github.com/repos/${REPO_OWNER}/${REPO_NAME}`;

// Below this count a bare number doesn't read as social proof, so the badge
// shows a plain "Star" prompt instead.
export const GITHUB_STAR_THRESHOLD = 10;

// `website/static/install.sh` is the same file as the repository's tested
// `scripts/install.sh`. The parity gate refuses drift between the command
// copied here and the installer contributors run from a clone.
//
// One method, so one command: the Homebrew and Nix tabs come back when a
// formula and a flake actually exist to put behind them.
export const INSTALL_COMMAND = 'curl -fsSL https://www.usecyclops.dev/install.sh | sh';
export const INSTALL_NOTE =
	'Builds from source: needs tmux 3.2+. No Rust toolchain? The installer gets one from rustup.rs itself.';

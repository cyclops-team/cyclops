export const REPO_OWNER = 'cyclops-team';
export const REPO_NAME = 'cyclops';
export const REPO_URL = `https://github.com/${REPO_OWNER}/${REPO_NAME}`;
export const DOCS_URL = 'https://www.usecyclops.dev/docs';
export const LICENSE_URL = `${REPO_URL}/blob/main/LICENSE`;
export const GITHUB_API_URL = `https://api.github.com/repos/${REPO_OWNER}/${REPO_NAME}`;

// Below this count a bare number doesn't read as social proof, so the badge
// shows a plain "Star" prompt instead.
export const GITHUB_STAR_THRESHOLD = 10;

export type InstallStatus = 'live' | 'coming-soon';

export interface InstallMethod {
	id: 'script' | 'homebrew' | 'nix';
	label: string;
	status: InstallStatus;
	lines?: string[];
	note?: string;
}

// `website/static/install.sh` is the same file as the repository's tested
// `scripts/install.sh`. The parity gate refuses drift between the command
// copied here and the installer contributors run from a clone.
export const INSTALL_METHODS: InstallMethod[] = [
	{
		id: 'script',
		label: 'Script',
		status: 'live',
		lines: ['curl -fsSL https://www.usecyclops.dev/install.sh | sh'],
		note: 'Builds from source: needs tmux 3.2+ and a Rust toolchain. No cargo? Get Rust from rustup.rs, not Homebrew.'
	},
	{
		id: 'homebrew',
		label: 'Homebrew',
		status: 'coming-soon',
		note: 'No formula published yet.'
	},
	{
		id: 'nix',
		label: 'Nix',
		status: 'coming-soon',
		note: 'No flake published yet.'
	}
];

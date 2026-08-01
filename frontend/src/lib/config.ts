export const REPO_OWNER = 'notyahir';
export const REPO_NAME = 'cyclops';
export const REPO_URL = `https://github.com/${REPO_OWNER}/${REPO_NAME}`;
export const DOCS_URL = `${REPO_URL}/tree/main/docs`;
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

// The script itself now lives at frontend/static/install.sh (served at
// /install.sh by this SvelteKit app). It fetches the release from GitHub
// and delegates the actual install to bin/commPact-install, which stays
// network-free — see tests/regression.sh for that guarantee.
// Still TODO: confirm usecyclops.dev is actually deployed and pointed at
// this app before shipping; otherwise "Copy" hands users a dead command.
export const INSTALL_METHODS: InstallMethod[] = [
	{
		id: 'script',
		label: 'Script',
		status: 'live',
		lines: ['curl -fsSL https://usecyclops.dev/install.sh | sh']
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

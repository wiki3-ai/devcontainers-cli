// entry.ts
//
// Perry entry point. Reads exactly one request envelope from stdin (NDJSON),
// services any number of host calls, and writes exactly one result (or one
// error) envelope to stdout before exiting.
//
// See ../PROTOCOL.md for the wire contract.

import * as jsonc from 'jsonc-parser';
import { URI } from 'vscode-uri';

import { createPerryFileHost, FileHost, HostChannel, setChannel } from './host';
import {
	DevContainerConfig,
	updateFromOldProperties,
	uriToFsPath,
	getWellKnownDevContainerPaths,
	substitute,
	SubstitutionContext,
} from './re-exports';

const PROTOCOL_VERSION = 1;
const SLICE_ID = 'devcontainers-cli@0.86.0';

interface Request {
	kind: 'request';
	command: 'loadConfig';
	workspaceFolder: string;
	configFile?: string;
	platform: NodeJS.Platform;
	env: Record<string, string>;
}

// ---------------------------------------------------------------------------
// stdio plumbing
// ---------------------------------------------------------------------------

function makeStdioChannel(): HostChannel {
	const stdin = process.stdin;
	stdin.setEncoding('utf8');
	let buffer = '';
	const queue: string[] = [];
	const waiters: ((line: string) => void)[] = [];

	stdin.on('data', (chunk: string) => {
		buffer += chunk;
		let nl: number;
		// eslint-disable-next-line no-cond-assign
		while ((nl = buffer.indexOf('\n')) !== -1) {
			const line = buffer.slice(0, nl);
			buffer = buffer.slice(nl + 1);
			if (waiters.length > 0) {
				waiters.shift()!(line);
			} else {
				queue.push(line);
			}
		}
	});
	stdin.on('end', () => {
		while (waiters.length > 0) {
			waiters.shift()!('');
		}
	});

	return {
		send(line: string) {
			process.stdout.write(line + '\n');
		},
		recv() {
			if (queue.length > 0) {
				return Promise.resolve(queue.shift()!);
			}
			return new Promise<string>(resolve => waiters.push(resolve));
		},
	};
}

// recv() in the channel is shared between the request reader and the
// host-reply reader. We separate concerns by pulling the very first line
// (the request) before installing the channel for host.ts.
async function readSingleLine(channel: HostChannel): Promise<string> {
	return channel.recv();
}

// ---------------------------------------------------------------------------
// Slice entry
// ---------------------------------------------------------------------------

async function loadConfig(req: Request, fileHost: FileHost): Promise<unknown> {
	const platform = req.platform;
	const p = platform === 'win32' ? fileHost.path.win32 ?? fileHost.path : fileHost.path;
	void p; // currently unused; reserved for future per-platform path handling

	// Resolve the config file path.
	let configPathFs: string | undefined = req.configFile;
	if (!configPathFs) {
		for (const candidate of getWellKnownDevContainerPaths(fileHost.path, req.workspaceFolder)) {
			if (await fileHost.isFile(candidate)) {
				configPathFs = candidate;
				break;
			}
		}
	}
	if (!configPathFs) {
		throw new Error(`Dev container config not found under ${req.workspaceFolder}`);
	}

	const configFile = URI.file(configPathFs);

	// Read + JSONC-parse.
	const buffer = await fileHost.readFile(configPathFs);
	const content = buffer.toString();
	const raw = jsonc.parse(content) as DevContainerConfig | undefined;
	if (!raw || typeof raw !== 'object' || Array.isArray(raw)) {
		throw new Error(
			`Dev container config (${uriToFsPath(configFile, platform)}) must contain a JSON object literal.`,
		);
	}

	// updateFromOldProperties expects the broader VSCode-customization shape
	// but tolerates configs without it; cast through unknown.
	const updated = updateFromOldProperties(raw as never);

	// Pre-container variable substitution.
	const ctx: SubstitutionContext = {
		platform,
		configFile,
		localWorkspaceFolder: req.workspaceFolder,
		env: req.env as NodeJS.ProcessEnv,
	};
	const substituted = substitute(ctx, updated);
	(substituted as DevContainerConfig).configFilePath = configFile;

	return {
		config: substituted,
		raw: updated,
		configFilePath: configPathFs,
	};
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function main(): Promise<void> {
	process.stdout.write(JSON.stringify({ kind: 'hello', protocol: PROTOCOL_VERSION, slice: SLICE_ID }) + '\n');

	const channel = makeStdioChannel();
	const requestLine = await readSingleLine(channel);
	if (!requestLine) {
		throw new Error('no request received on stdin');
	}
	const req = JSON.parse(requestLine) as Request;
	if (req.kind !== 'request') {
		throw new Error(`unexpected envelope kind: ${(req as { kind: string }).kind}`);
	}

	// From here on, every line from stdin is a host-reply. Install the
	// channel for host.ts to drive its reader loop.
	setChannel(channel);

	const fileHost = createPerryFileHost(req.platform);

	let value: unknown;
	switch (req.command) {
		case 'loadConfig':
			value = await loadConfig(req, fileHost);
			break;
		default:
			throw new Error(`unknown command: ${(req as { command: string }).command}`);
	}

	process.stdout.write(JSON.stringify({ kind: 'result', value }) + '\n');
}

main().then(
	() => process.exit(0),
	err => {
		const message = err && err.message ? String(err.message) : String(err);
		const stack = err && err.stack ? String(err.stack) : undefined;
		process.stdout.write(JSON.stringify({ kind: 'error', message, stack }) + '\n');
		process.exit(1);
	},
);

/*---------------------------------------------------------------------------------------------
 *  Devcontainer engine entry point loaded by the Tauri WebView.
 *
 *  Re-exports the spec slice with a `FileHost` adapter that proxies all
 *  filesystem reads/writes to Tauri commands. The spec slice itself is pure
 *  TS (no Node built-ins beyond `node:path` / `node:crypto` which Deno
 *  inlines at bundle time).
 *
 *  Step 6 of the conversion roadmap: this module also provides the
 *  end-to-end `loadDevContainerConfig` pipeline — read the workspace's
 *  `.devcontainer/devcontainer.json` through the `FileHost`, parse JSONC,
 *  run pre-container variable substitution, and post a runtime-agnostic
 *  parsed shape back to the Rust host.
 *--------------------------------------------------------------------------------------------*/

import * as posixPath from 'node:path';
import * as jsonc from 'jsonc-parser';

import {
	CLIHostDocuments,
	type Documents,
	createDocuments,
} from '../../../spec/spec-configuration/editableFiles';
import {
	getDevContainerConfigPathIn,
	type FileHost,
} from '../../../spec/spec-configuration/configurationCommonUtils';
import { workspaceFromPath } from '../../../spec/spec-utils/workspaces';
import {
	substitute,
	beforeContainerSubstitute,
	type SubstitutionContext,
} from '../../../spec/spec-common/variableSubstitution';
import type { DevContainerConfig, Mount } from '../../../spec/spec-configuration/configuration';
import { bridge } from '../lib/bridge';

export {
	CLIHostDocuments,
	createDocuments,
	getDevContainerConfigPathIn,
	workspaceFromPath,
	substitute,
	beforeContainerSubstitute,
};
export type { Documents, FileHost, SubstitutionContext, DevContainerConfig };

// FileHost implementation that bridges to the Rust host via Tauri commands.
// All reads/writes are sandboxed by the Rust side to the active workspace.
export function tauriFileHost(): FileHost {
	return {
		platform: 'darwin', // The app only ships on macOS for v1.
		path: posixPath.posix,
		isFile: (p) => bridge.fs_is_file(p),
		readFile: async (p) => new Uint8Array(await bridge.fs_read_file(p)),
		writeFile: (p, content) => bridge.fs_write_file(p, Array.from(content)),
		readDir: (p) => bridge.fs_read_dir(p),
		mkdirp: (p) => bridge.fs_mkdirp(p),
		toCommonURI: async () => undefined,
	};
}

/**
 * Runtime-agnostic shape posted to the Rust host. Mirrors
 * `crate::devcontainer::translate::ParsedDevContainer` field-for-field;
 * the Rust orchestrator translates this into a `ContainerSpec`.
 *
 * Field naming matches Rust's `serde(rename_all = "snake_case")` default
 * since `tauri::command` rewrites camelCase JS keys to snake_case
 * automatically.
 */
export interface ParsedDevContainer {
	name?: string;
	image?: string;
	workspaceFolder?: string;
	workspaceMount?: string;
	mounts: string[];
	forwardPorts: number[];
	remoteUser?: string;
	containerEnv: Record<string, string>;
	remoteEnv: Record<string, string | null>;
	onCreateCommand?: string | string[];
	updateContentCommand?: string | string[];
	postCreateCommand?: string | string[];
	postStartCommand?: string | string[];
	postAttachCommand?: string | string[];
}

export interface LoadConfigResult {
	configFilePath: string;
	parsed: ParsedDevContainer;
	raw: DevContainerConfig;
}

/**
 * Load, parse, and pre-container-substitute the `.devcontainer/devcontainer.json`
 * for `workspacePath`. Returns `undefined` if no config file exists.
 *
 * The function is pure with respect to `fileHost`, so unit tests can pass
 * an in-memory implementation without going through Tauri.
 */
export async function loadDevContainerConfig(
	fileHost: FileHost,
	workspacePath: string,
	env: Record<string, string | undefined> = {},
): Promise<LoadConfigResult | undefined> {
	const ws = workspaceFromPath(fileHost.path, workspacePath);
	const uri = await getDevContainerConfigPathIn(fileHost, ws.configFolderPath);
	if (!uri) {
		return undefined;
	}
	const documents = createDocuments(fileHost);
	const text = await documents.readDocument(uri);
	if (text === undefined) {
		return undefined;
	}
	const errors: jsonc.ParseError[] = [];
	const tree = jsonc.parse(text, errors, { allowTrailingComma: true });
	if (errors.length > 0) {
		const first = errors[0];
		throw new Error(`devcontainer.json parse error: ${jsonc.printParseErrorCode(first.error)} at offset ${first.offset}`);
	}
	const config = tree as DevContainerConfig;
	(config as DevContainerConfig & { configFilePath?: unknown }).configFilePath = uri;

	const ctx: SubstitutionContext = {
		platform: fileHost.platform,
		configFile: uri,
		localWorkspaceFolder: ws.rootFolderPath,
		containerWorkspaceFolder: undefined,
		env,
	};
	const substituted = substitute(ctx, config);

	return {
		configFilePath: uri.toString(),
		parsed: toParsed(substituted),
		raw: substituted,
	};
}

/**
 * Project a fully-substituted `DevContainerConfig` onto the minimal shape
 * the Rust orchestrator consumes. Features, dockerComposeFile, and
 * dockerFile-based configs land in later phases — for v1 the orchestrator
 * only consumes image-based configs.
 */
export function toParsed(config: DevContainerConfig): ParsedDevContainer {
	const c = config as DevContainerConfig & {
		image?: string;
		mounts?: (Mount | string)[];
		forwardPorts?: (number | string)[];
		workspaceFolder?: string;
		workspaceMount?: string;
		remoteUser?: string;
		containerEnv?: Record<string, string>;
		remoteEnv?: Record<string, string | null>;
		onCreateCommand?: string | string[];
		updateContentCommand?: string | string[];
		postCreateCommand?: string | string[];
		postStartCommand?: string | string[];
		postAttachCommand?: string | string[];
	};
	const mounts = (c.mounts ?? []).map((m) =>
		typeof m === 'string' ? m : mountToString(m),
	);
	const forwardPorts = (c.forwardPorts ?? [])
		.map((p) => (typeof p === 'number' ? p : Number.parseInt(p, 10)))
		.filter((p) => Number.isFinite(p) && p > 0 && p < 65536);
	return {
		name: c.name,
		image: c.image,
		workspaceFolder: c.workspaceFolder,
		workspaceMount: c.workspaceMount,
		mounts,
		forwardPorts,
		remoteUser: c.remoteUser,
		containerEnv: c.containerEnv ?? {},
		remoteEnv: c.remoteEnv ?? {},
		onCreateCommand: c.onCreateCommand,
		updateContentCommand: c.updateContentCommand,
		postCreateCommand: c.postCreateCommand,
		postStartCommand: c.postStartCommand,
		postAttachCommand: c.postAttachCommand,
	};
}

function mountToString(m: Mount): string {
	const parts: string[] = [`type=${m.type}`];
	if (m.source) {
		parts.push(`source=${m.source}`);
	}
	parts.push(`target=${m.target}`);
	return parts.join(',');
}

/**
 * Convenience wrapper used by the dashboard UI: load + post + return.
 * Rust associates the parsed config with `workspaceId` so subsequent
 * lifecycle commands (`container_up`, etc.) can pick it up.
 */
export async function loadAndSubmitDevContainerConfig(
	fileHost: FileHost,
	workspaceId: string,
	workspacePath: string,
	env: Record<string, string | undefined> = {},
): Promise<LoadConfigResult | undefined> {
	const result = await loadDevContainerConfig(fileHost, workspacePath, env);
	if (!result) {
		return undefined;
	}
	await bridge.submit_parsed_devcontainer(workspaceId, result.parsed);
	return result;
}


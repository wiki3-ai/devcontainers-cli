/*---------------------------------------------------------------------------------------------
 *  Devcontainer engine entry point loaded by the Tauri WebView.
 *
 *  Re-exports the spec slice with a `FileHost` adapter that proxies all
 *  filesystem reads/writes to Tauri commands. The spec slice itself is pure
 *  TS (no Node built-ins beyond `node:path` / `node:crypto` which Deno
 *  inlines at bundle time).
 *--------------------------------------------------------------------------------------------*/

import * as posixPath from 'node:path';

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
import type { DevContainerConfig } from '../../../spec/spec-configuration/configuration';
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

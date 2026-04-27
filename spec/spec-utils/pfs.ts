/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

// FileHost-only slice of pfs.ts. No Node `fs` / `ncp` imports. The real
// filesystem implementation is provided at runtime by the Tauri host; tests
// may supply an in-memory or Node-backed implementation.

import type * as path from 'node:path';
import type { URI } from 'vscode-uri';

export interface FileHost {
	platform: NodeJS.Platform;
	path: typeof path.posix | typeof path.win32;
	isFile(filepath: string): Promise<boolean>;
	readFile(filepath: string): Promise<Uint8Array>;
	writeFile(filepath: string, content: Uint8Array): Promise<void>;
	readDir(dirpath: string): Promise<string[]>;
	readDirWithTypes?(dirpath: string): Promise<[string, FileTypeBitmask][]>;
	mkdirp(dirpath: string): Promise<void>;
	toCommonURI(filePath: string): Promise<URI | undefined>;
}

export enum FileTypeBitmask {
	Unknown = 0,
	File = 1,
	Directory = 2,
	SymbolicLink = 64
}

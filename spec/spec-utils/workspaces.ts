/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import type * as path from 'node:path';

export interface Workspace {
	readonly isWorkspaceFile: boolean;
	readonly workspaceOrFolderPath: string;
	readonly rootFolderPath: string;
	readonly configFolderPath: string;
}

export function workspaceFromPath(path_: typeof path.posix | typeof path.win32, workspaceOrFolderPath: string): Workspace {
	if (isWorkspacePath(workspaceOrFolderPath)) {
		const workspaceFolder = path_.dirname(workspaceOrFolderPath);
		return {
			isWorkspaceFile: true,
			workspaceOrFolderPath,
			rootFolderPath: workspaceFolder,
			configFolderPath: workspaceFolder,
		};
	}
	return {
		isWorkspaceFile: false,
		workspaceOrFolderPath,
		rootFolderPath: workspaceOrFolderPath,
		configFolderPath: workspaceOrFolderPath,
	};
}

const WORKSPACE_FILE_SUFFIX = '.code-workspace';

export function isWorkspacePath(workspaceOrFolderPath: string) {
	// String-only test: avoids dragging `node:path.extname` into the spec
	// slice. The slice never imports a runtime `path` module — all path
	// operations come through the `FileHost.path` injected by the host.
	return workspaceOrFolderPath.endsWith(WORKSPACE_FILE_SUFFIX);
}

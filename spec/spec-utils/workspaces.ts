/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import * as path from 'node:path';

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

export function isWorkspacePath(workspaceOrFolderPath: string) {
	return path.extname(workspaceOrFolderPath) === '.code-workspace';
}

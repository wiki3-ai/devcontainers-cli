/*---------------------------------------------------------------------------------------------
 *  Typed bridge between the WebView frontend and the Tauri Rust host.
 *
 *  Mirrors the layering of `wiki3-ai/wiki3-app`'s `src/lib/bridge.ts`. All
 *  IPC goes through `invoke` and `listen` from `@tauri-apps/api`; never
 *  reach into the Tauri global directly from UI components.
 *--------------------------------------------------------------------------------------------*/

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

export interface WorkspaceEntry {
	id: string;
	path: string;
	displayName: string;
	lastOpenedAt?: string;
}

export type ContainerState =
	| 'absent'
	| 'pulling'
	| 'creating'
	| 'created'
	| 'running'
	| 'stopped'
	| 'exited'
	| 'unknown'
	| 'error';

export interface ContainerStatus {
	workspaceId: string;
	state: ContainerState;
	containerId?: string;
	imageRef?: string;
	error?: string;
}

export interface RuntimeInfo {
	id: 'apple-containers' | 'podman' | 'docker';
	available: boolean;
	version?: string;
	reason?: string;
}

export interface LifecycleLogEvent {
	workspaceId: string;
	stream: 'stdout' | 'stderr' | 'system';
	line: string;
	ts: number;
}

/**
 * Strongly typed wrappers around `invoke`. Kept as an object so tests can
 * monkey-patch individual methods, and so the FileHost adapter in
 * `devcontainer-engine` can depend on this surface without importing Tauri
 * APIs directly.
 */
export const bridge = {
	// --- workspace management -----------------------------------------------
	list_workspaces: () => invoke<WorkspaceEntry[]>('list_workspaces'),
	add_workspace: (path: string) => invoke<WorkspaceEntry>('add_workspace', { path }),
	remove_workspace: (id: string) => invoke<void>('remove_workspace', { id }),

	// --- runtime ------------------------------------------------------------
	list_runtimes: () => invoke<RuntimeInfo[]>('list_runtimes'),
	select_runtime: (id: RuntimeInfo['id']) => invoke<void>('select_runtime', { id }),

	// --- container lifecycle ------------------------------------------------
	container_status: (workspaceId: string) => invoke<ContainerStatus>('container_status', { workspaceId }),
	container_up: (workspaceId: string) => invoke<ContainerStatus>('container_up', { workspaceId }),
	container_stop: (workspaceId: string) => invoke<ContainerStatus>('container_stop', { workspaceId }),
	container_rebuild: (workspaceId: string) => invoke<ContainerStatus>('container_rebuild', { workspaceId }),
	container_remove: (workspaceId: string) => invoke<ContainerStatus>('container_remove', { workspaceId }),

	// --- parsed devcontainer.json ------------------------------------------
	submit_parsed_devcontainer: (workspaceId: string, parsed: unknown) =>
		invoke<void>('submit_parsed_devcontainer', { workspaceId, parsed }),

	// --- FileHost bridge ----------------------------------------------------
	fs_is_file: (path: string) => invoke<boolean>('fs_is_file', { path }),
	fs_read_file: (path: string) => invoke<number[]>('fs_read_file', { path }),
	fs_write_file: (path: string, content: number[]) => invoke<void>('fs_write_file', { path, content }),
	fs_read_dir: (path: string) => invoke<string[]>('fs_read_dir', { path }),
	fs_mkdirp: (path: string) => invoke<void>('fs_mkdirp', { path }),

	// --- events -------------------------------------------------------------
	onLifecycleLog(handler: (e: LifecycleLogEvent) => void): Promise<UnlistenFn> {
		return listen<LifecycleLogEvent>('devcontainer://log', (e) => handler(e.payload));
	},
	onStatusChange(handler: (s: ContainerStatus) => void): Promise<UnlistenFn> {
		return listen<ContainerStatus>('devcontainer://status', (e) => handler(e.payload));
	},
};

export type Bridge = typeof bridge;

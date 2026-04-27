/*---------------------------------------------------------------------------------------------
 *  Devcontainers.app — dashboard entry point.
 *
 *  Step 8 of the conversion roadmap: minimal MVP UI with the dashboard,
 *  "Open Folder…" action, per-workspace detail panel (Up / Stop / Rebuild /
 *  Remove), and an xterm.js log pane subscribed to `devcontainer://log` and
 *  `devcontainer://status` events emitted by the Rust orchestrator.
 *--------------------------------------------------------------------------------------------*/

import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import '@xterm/xterm/css/xterm.css';

import {
	bridge,
	type ContainerStatus,
	type LifecycleLogEvent,
	type RuntimeInfo,
	type WorkspaceEntry,
} from './lib/bridge';

// Type-only reference to the engine module so the editor and (where it
// runs) tsc validate our usage. The actual JS comes from the deno-built
// bundle that lives in `frontend/public/devcontainer-engine.js` and is
// loaded at runtime via dynamic import. `@vite-ignore` keeps Vite from
// trying to resolve and bundle the spec slice itself, which uses
// `node:path` and other Node built-ins that the deno bundler polyfills
// but Vite does not.
type EngineModule = typeof import('./devcontainer-engine');
let enginePromise: Promise<EngineModule> | undefined;
function engine(): Promise<EngineModule> {
	if (!enginePromise) {
		enginePromise = import(/* @vite-ignore */ '/devcontainer-engine.js') as Promise<EngineModule>;
	}
	return enginePromise;
}

interface AppState {
	workspaces: WorkspaceEntry[];
	runtimes: RuntimeInfo[];
	selected?: string;
	statuses: Record<string, ContainerStatus | undefined>;
}

const state: AppState = {
	workspaces: [],
	runtimes: [],
	selected: undefined,
	statuses: {},
};

let terminal: Terminal | undefined;
let fit: FitAddon | undefined;

async function bootstrap(): Promise<void> {
	const root = document.getElementById('app');
	if (!root) {
		return;
	}
	root.textContent = 'Loading workspaces…';
	try {
		[state.workspaces, state.runtimes] = await Promise.all([
			bridge.list_workspaces(),
			bridge.list_runtimes(),
		]);
	} catch (err) {
		root.textContent = `Failed to load: ${(err as Error).message}`;
		return;
	}

	render(root);
	subscribe();
}

function subscribe(): void {
	void bridge.onLifecycleLog((e) => {
		if (e.workspaceId !== state.selected) {
			return;
		}
		writeLog(e);
	});
	void bridge.onStatusChange((s) => {
		state.statuses[s.workspaceId] = s;
		const root = document.getElementById('app');
		if (root) {
			render(root);
		}
	});
}

function render(root: HTMLElement): void {
	root.replaceChildren();

	const header = document.createElement('header');
	header.className = 'app-header';
	const heading = document.createElement('h1');
	heading.textContent = 'Devcontainers.app';
	header.appendChild(heading);

	const openBtn = document.createElement('button');
	openBtn.textContent = 'Open Folder…';
	openBtn.addEventListener('click', onOpenFolder);
	header.appendChild(openBtn);
	root.appendChild(header);

	root.appendChild(renderRuntimes());

	const layout = document.createElement('section');
	layout.className = 'layout';
	layout.appendChild(renderSidebar());
	layout.appendChild(renderDetail());
	root.appendChild(layout);
}

function renderRuntimes(): HTMLElement {
	const ul = document.createElement('ul');
	ul.className = 'runtimes';
	for (const r of state.runtimes) {
		const li = document.createElement('li');
		li.textContent = `${r.id}: ${r.available ? (r.version ?? 'available') : (r.reason ?? 'unavailable')}`;
		ul.appendChild(li);
	}
	return ul;
}

function renderSidebar(): HTMLElement {
	const aside = document.createElement('aside');
	aside.className = 'sidebar';

	if (state.workspaces.length === 0) {
		const empty = document.createElement('p');
		empty.textContent = 'No workspaces yet. Click "Open Folder…" to add one.';
		aside.appendChild(empty);
		return aside;
	}

	const ul = document.createElement('ul');
	ul.className = 'workspaces';
	for (const w of state.workspaces) {
		const li = document.createElement('li');
		li.dataset.id = w.id;
		if (w.id === state.selected) {
			li.classList.add('selected');
		}
		const title = document.createElement('div');
		title.className = 'ws-title';
		title.textContent = w.displayName;
		const path = document.createElement('div');
		path.className = 'ws-path';
		path.textContent = w.path;
		const status = document.createElement('div');
		status.className = 'ws-status';
		status.textContent = state.statuses[w.id]?.state ?? 'absent';
		li.append(title, path, status);
		li.addEventListener('click', () => selectWorkspace(w.id));
		ul.appendChild(li);
	}
	aside.appendChild(ul);
	return aside;
}

function renderDetail(): HTMLElement {
	const main = document.createElement('section');
	main.className = 'detail';

	const ws = state.workspaces.find((w) => w.id === state.selected);
	if (!ws) {
		const empty = document.createElement('p');
		empty.textContent = 'Select a workspace to manage its dev container.';
		main.appendChild(empty);
		return main;
	}

	const header = document.createElement('div');
	header.className = 'detail-header';
	const title = document.createElement('h2');
	title.textContent = ws.displayName;
	header.appendChild(title);

	const actions = document.createElement('div');
	actions.className = 'actions';
	for (const [label, fn] of [
		['Up', () => action('up', ws.id)],
		['Stop', () => action('stop', ws.id)],
		['Rebuild', () => action('rebuild', ws.id)],
		['Remove', () => action('remove', ws.id)],
	] as const) {
		const btn = document.createElement('button');
		btn.textContent = label;
		btn.addEventListener('click', () => void fn());
		actions.appendChild(btn);
	}
	header.appendChild(actions);
	main.appendChild(header);

	const status = state.statuses[ws.id];
	const statusLine = document.createElement('p');
	statusLine.className = 'status-line';
	statusLine.textContent = status
		? `${status.state}${status.containerId ? ` — ${status.containerId}` : ''}${status.error ? ` (error: ${status.error})` : ''}`
		: 'absent';
	main.appendChild(statusLine);

	const term = document.createElement('div');
	term.className = 'terminal';
	term.id = 'terminal';
	main.appendChild(term);

	queueMicrotask(() => mountTerminal(term));
	return main;
}

function mountTerminal(host: HTMLElement): void {
	terminal?.dispose();
	terminal = new Terminal({
		convertEol: true,
		fontFamily: 'Menlo, Consolas, monospace',
		fontSize: 12,
		theme: { background: '#1e1e1e' },
	});
	fit = new FitAddon();
	terminal.loadAddon(fit);
	terminal.open(host);
	try {
		fit.fit();
	} catch {
		// xterm is sensitive to host dimensions; ignore until the next layout.
	}
}

function writeLog(e: LifecycleLogEvent): void {
	if (!terminal) {
		return;
	}
	const prefix =
		e.stream === 'stderr' ? '\x1b[31m' : e.stream === 'system' ? '\x1b[36m' : '';
	const reset = prefix ? '\x1b[0m' : '';
	terminal.writeln(`${prefix}${e.line}${reset}`);
}

function selectWorkspace(id: string): void {
	state.selected = id;
	const root = document.getElementById('app');
	if (root) {
		render(root);
	}
	void refreshStatus(id);
	void loadConfigForWorkspace(id);
}

async function refreshStatus(id: string): Promise<void> {
	try {
		state.statuses[id] = await bridge.container_status(id);
	} catch (err) {
		// Stale status is acceptable; surface in the UI on next render.
		state.statuses[id] = {
			workspaceId: id,
			state: 'error',
			error: (err as Error).message,
		};
	}
	const root = document.getElementById('app');
	if (root) {
		render(root);
	}
}

async function loadConfigForWorkspace(id: string): Promise<void> {
	const ws = state.workspaces.find((w) => w.id === id);
	if (!ws) {
		return;
	}
	try {
		const eng = await engine();
		const result = await eng.loadAndSubmitDevContainerConfig(
			eng.tauriFileHost(),
			id,
			ws.path,
		);
		if (!result) {
			terminal?.writeln(
				`\x1b[33mNo .devcontainer/devcontainer.json found in ${ws.path}\x1b[0m`,
			);
		}
	} catch (err) {
		terminal?.writeln(`\x1b[31mFailed to load devcontainer.json: ${(err as Error).message}\x1b[0m`);
	}
}

async function onOpenFolder(): Promise<void> {
	const picked = await openDialog({ directory: true, multiple: false });
	if (typeof picked !== 'string') {
		return;
	}
	try {
		const ws = await bridge.add_workspace(picked);
		state.workspaces.push(ws);
		state.selected = ws.id;
	} catch (err) {
		terminal?.writeln(
			`\x1b[31mFailed to add workspace: ${(err as Error).message}\x1b[0m`,
		);
		state.statuses[picked] = {
			workspaceId: picked,
			state: 'error',
			error: (err as Error).message,
		};
		const root = document.getElementById('app');
		if (root) {
			render(root);
		}
		return;
	}
	const root = document.getElementById('app');
	if (root) {
		render(root);
	}
}

type LifecycleAction = 'up' | 'stop' | 'rebuild' | 'remove';

async function action(kind: LifecycleAction, workspaceId: string): Promise<void> {
	try {
		const fn = {
			up: bridge.container_up,
			stop: bridge.container_stop,
			rebuild: bridge.container_rebuild,
			remove: bridge.container_remove,
		}[kind];
		const status = await fn(workspaceId);
		state.statuses[workspaceId] = status;
	} catch (err) {
		state.statuses[workspaceId] = {
			workspaceId,
			state: 'error',
			error: (err as Error).message,
		};
	}
	const root = document.getElementById('app');
	if (root) {
		render(root);
	}
}

void bootstrap();


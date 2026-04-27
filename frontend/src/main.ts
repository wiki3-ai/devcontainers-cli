/*---------------------------------------------------------------------------------------------
 *  Devcontainers.app — dashboard entry point.
 *
 *  Minimal MVP UI: dashboard, "Open Folder…" action, per-workspace detail
 *  panel (Up / Stop / Rebuild / Remove), and a plain-text log pane
 *  subscribed to `devcontainer://log` and `devcontainer://status` events
 *  emitted by the Rust orchestrator. Logs are buffered per-workspace so
 *  they survive re-renders and keep accumulating until the workspace is
 *  removed from the dashboard.
 *--------------------------------------------------------------------------------------------*/

import { open as openDialog } from '@tauri-apps/plugin-dialog';

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
// loaded at runtime.
//
// We can't dynamic-`import('/devcontainer-engine.js')` directly: Vite 7
// refuses to serve files under `/public/` as modules (they're meant to be
// raw static assets), and `@vite-ignore` does not bypass that check. So
// we fetch the bundle as text, wrap it in a blob URL, and import that.
// The blob is opaque to Vite's plugin pipeline, and the file is still
// served verbatim from `/public/` (dev) or `dist/` (build).
type EngineModule = typeof import('./devcontainer-engine');
let enginePromise: Promise<EngineModule> | undefined;
function engine(): Promise<EngineModule> {
	if (!enginePromise) {
		enginePromise = (async () => {
			const res = await fetch('/devcontainer-engine.js');
			if (!res.ok) {
				throw new Error(`Failed to fetch engine bundle: ${res.status} ${res.statusText}`);
			}
			const code = await res.text();
			const url = URL.createObjectURL(new Blob([code], { type: 'text/javascript' }));
			try {
				return (await import(/* @vite-ignore */ url)) as EngineModule;
			} finally {
				URL.revokeObjectURL(url);
			}
		})();
	}
	return enginePromise;
}

interface AppState {
	workspaces: WorkspaceEntry[];
	runtimes: RuntimeInfo[];
	selected?: string;
	statuses: Record<string, ContainerStatus | undefined>;
	/** Per-workspace log lines, in arrival order, capped at MAX_LOG_LINES. */
	logs: Map<string, LogLine[]>;
}

interface LogLine {
	stream: 'stdout' | 'stderr' | 'system';
	line: string;
	ts: number;
}

const MAX_LOG_LINES = 5000;

const state: AppState = {
	workspaces: [],
	runtimes: [],
	selected: undefined,
	statuses: {},
	logs: new Map(),
};

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
		appendLog(e);
		if (e.workspaceId === state.selected) {
			appendLogToPane(e);
		}
	});
	void bridge.onStatusChange((s) => {
		state.statuses[s.workspaceId] = s;
		updateStatusUi(s.workspaceId);
	});
}

/** Append a log line to the per-workspace ring buffer (capped). */
function appendLog(e: LifecycleLogEvent): void {
	let buf = state.logs.get(e.workspaceId);
	if (!buf) {
		buf = [];
		state.logs.set(e.workspaceId, buf);
	}
	buf.push({ stream: e.stream, line: e.line, ts: e.ts });
	if (buf.length > MAX_LOG_LINES) {
		buf.splice(0, buf.length - MAX_LOG_LINES);
	}
}

/** Update only the bits of the UI that depend on a workspace's status,
 *  without tearing down the log pane. */
function updateStatusUi(workspaceId: string): void {
	const s = state.statuses[workspaceId];
	const sidebarStatus = document.querySelector<HTMLElement>(
		`li[data-id="${cssEscape(workspaceId)}"] .ws-status`,
	);
	if (sidebarStatus) {
		sidebarStatus.textContent = s?.state ?? 'absent';
	}
	if (workspaceId === state.selected) {
		const statusLine = document.getElementById('status-line');
		if (statusLine && s) {
			statusLine.textContent = formatStatus(s);
		} else if (statusLine) {
			statusLine.textContent = 'absent';
		}
	}
}

function cssEscape(s: string): string {
	return s.replace(/["\\]/g, '\\$&');
}

function formatStatus(s: ContainerStatus): string {
	const parts: string[] = [s.state];
	if (s.containerId) parts.push(s.containerId);
	if (s.error) parts.push(`error: ${s.error}`);
	return parts.join(' \u2014 ');
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
	statusLine.id = 'status-line';
	statusLine.textContent = status ? formatStatus(status) : 'absent';
	main.appendChild(statusLine);

	const pane = document.createElement('pre');
	pane.className = 'log-pane';
	pane.id = 'log-pane';
	renderLogBuffer(pane, ws.id);
	main.appendChild(pane);
	return main;
}

/** Replay the buffered log lines for a workspace into the pane. */
function renderLogBuffer(pane: HTMLElement, workspaceId: string): void {
	pane.replaceChildren();
	const buf = state.logs.get(workspaceId);
	if (!buf || buf.length === 0) {
		return;
	}
	const frag = document.createDocumentFragment();
	for (const entry of buf) {
		frag.appendChild(makeLogLineNode(entry));
	}
	pane.appendChild(frag);
	// Defer to layout so the pane has its scroll height.
	queueMicrotask(() => {
		pane.scrollTop = pane.scrollHeight;
	});
}

/** Append a single log line to the visible pane (without re-rendering
 *  the entire buffer). Auto-scrolls only if the user is at/near the
 *  bottom, so they can scroll up to read without being yanked back. */
function appendLogToPane(e: LifecycleLogEvent): void {
	const pane = document.getElementById('log-pane');
	if (!pane) return;
	const nearBottom = pane.scrollHeight - pane.scrollTop - pane.clientHeight < 40;
	pane.appendChild(makeLogLineNode({ stream: e.stream, line: e.line, ts: e.ts }));
	// Trim DOM if we exceed the cap; cheaper than rebuilding the pane.
	while (pane.childElementCount > MAX_LOG_LINES) {
		pane.removeChild(pane.firstChild!);
	}
	if (nearBottom) {
		pane.scrollTop = pane.scrollHeight;
	}
}

function makeLogLineNode(entry: LogLine): HTMLElement {
	const row = document.createElement('div');
	row.className = `log-line log-${entry.stream}`;
	row.textContent = entry.line;
	return row;
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
			logLocal(id, 'system', `No .devcontainer/devcontainer.json found in ${ws.path}`);
		}
	} catch (err) {
		logLocal(id, 'stderr', `Failed to load devcontainer.json: ${(err as Error).message}`);
	}
}

/** Synthesize a log entry from the frontend (no Tauri event involved).
 *  Useful for surfacing errors that happen before/around lifecycle calls. */
function logLocal(workspaceId: string, stream: LogLine['stream'], line: string): void {
	const e: LifecycleLogEvent = { workspaceId, stream, line, ts: Date.now() };
	appendLog(e);
	if (workspaceId === state.selected) {
		appendLogToPane(e);
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
		logLocal(picked, 'stderr', `Failed to add workspace: ${(err as Error).message}`);
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
		logLocal(workspaceId, 'stderr', `${kind} failed: ${(err as Error).message}`);
	}
	// Don't full-rerender (would tear down the log pane); the orchestrator
	// already emits a `devcontainer://status` event that flows through
	// updateStatusUi(). Touch the sidebar/status text directly here too
	// so the UI doesn't flicker on the resolution of the action promise.
	updateStatusUi(workspaceId);
}

void bootstrap();


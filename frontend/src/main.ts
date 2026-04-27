/*---------------------------------------------------------------------------------------------
 *  Devcontainers.app — dashboard entry point.
 *
 *  Two-section dashboard, Podman-Desktop-style: a list of tracked repos
 *  (workspace folders) and a list of containers as known to the selected
 *  runtime. A repo is just dashboard bookkeeping; it can have a linked
 *  container (one per repo, by design) but doesn't require one.
 *
 *    Repo actions:       Up, Rebuild, Stop, Forget
 *    Container actions:  Start, Stop, Remove
 *
 *  "Forget" only removes the dashboard entry — the container, if any, is
 *  left alone. Container Remove force-deletes the container and refreshes
 *  the list. Logs are buffered per-entry so they survive sidebar
 *  selection changes and accumulate until the entry is removed.
 *--------------------------------------------------------------------------------------------*/

import { open as openDialog } from '@tauri-apps/plugin-dialog';

import {
	bridge,
	type ContainerEntry,
	type ContainerStatus,
	type LifecycleLogEvent,
	type RuntimeInfo,
	type WorkspaceEntry,
} from './lib/bridge';

// Type-only reference to the engine module so the editor and (where it
// runs) tsc validate our usage. The actual JS comes from the deno-built
// bundle that lives in `frontend/public/devcontainer-engine.js` and is
// loaded at runtime.
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

/** Tagged identity for a sidebar entry. */
type Selection =
	| { kind: 'repo'; id: string }
	| { kind: 'container'; id: string };

interface LogLine {
	stream: 'stdout' | 'stderr' | 'system';
	line: string;
	ts: number;
}

interface AppState {
	workspaces: WorkspaceEntry[];
	runtimes: RuntimeInfo[];
	containers: ContainerEntry[];
	/** Last known status per workspaceId (from lifecycle events). */
	statuses: Record<string, ContainerStatus | undefined>;
	selected?: Selection;
	/** Logs keyed by `repo:<wsId>` or `container:<cid>`. */
	logs: Map<string, LogLine[]>;
}

const MAX_LOG_LINES = 5000;

const state: AppState = {
	workspaces: [],
	runtimes: [],
	containers: [],
	statuses: {},
	selected: undefined,
	logs: new Map(),
};

// =============================================================================
// Bootstrap
// =============================================================================

async function bootstrap(): Promise<void> {
	const root = document.getElementById('app');
	if (!root) {
		return;
	}
	root.textContent = 'Loading…';
	try {
		[state.workspaces, state.runtimes, state.containers] = await Promise.all([
			bridge.list_workspaces(),
			bridge.list_runtimes(),
			bridge.list_containers().catch(() => []),
		]);
	} catch (err) {
		root.textContent = `Failed to load: ${errMsg(err)}`;
		return;
	}

	render(root);
	subscribe();
	startContainerPolling();
}

function subscribe(): void {
	void bridge.onLifecycleLog((e) => {
		const key = logKeyForWorkspace(e.workspaceId);
		appendLog(key, { stream: e.stream, line: e.line, ts: e.ts });
		if (selectionMatchesLogKey(key)) {
			appendLogToPane({ stream: e.stream, line: e.line, ts: e.ts });
		}
	});
	void bridge.onStatusChange((s) => {
		state.statuses[s.workspaceId] = s;
		updateRepoStatusUi(s.workspaceId);
		// A status change usually means a container appeared/disappeared.
		void refreshContainers();
	});
}

/** Poll the runtime's container list every few seconds so external
 *  changes (other tools, a `container kill`, etc) get reflected. */
function startContainerPolling(): void {
	setInterval(() => {
		void refreshContainers();
	}, 4000);
}

async function refreshContainers(): Promise<void> {
	try {
		state.containers = await bridge.list_containers();
	} catch {
		// Backend unavailable; keep the previous list. Avoid spamming
		// errors on a transient runtime hiccup.
		return;
	}
	renderSidebarOnly();
	if (state.selected?.kind === 'container') {
		const cid = state.selected.id;
		if (!state.containers.some((c) => c.containerId === cid)) {
			state.selected = undefined;
			renderDetailOnly();
		} else {
			updateContainerDetail();
		}
	}
}

// =============================================================================
// Logging
// =============================================================================

function logKeyForWorkspace(workspaceId: string): string {
	return `repo:${workspaceId}`;
}

function logKeyForContainer(containerId: string): string {
	return `container:${containerId}`;
}

function logKeyForSelection(sel: Selection): string {
	return sel.kind === 'repo' ? logKeyForWorkspace(sel.id) : logKeyForContainer(sel.id);
}

function selectionMatchesLogKey(key: string): boolean {
	return state.selected != null && logKeyForSelection(state.selected) === key;
}

function appendLog(key: string, entry: LogLine): void {
	let buf = state.logs.get(key);
	if (!buf) {
		buf = [];
		state.logs.set(key, buf);
	}
	buf.push(entry);
	if (buf.length > MAX_LOG_LINES) {
		buf.splice(0, buf.length - MAX_LOG_LINES);
	}
}

/** Synthesize a log entry from the frontend (no Tauri event involved). */
function logLocal(key: string, stream: LogLine['stream'], line: string): void {
	const entry: LogLine = { stream, line, ts: Date.now() };
	appendLog(key, entry);
	if (state.selected && logKeyForSelection(state.selected) === key) {
		appendLogToPane(entry);
	}
}

// =============================================================================
// Rendering
// =============================================================================

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
	const sidebar = renderSidebar();
	sidebar.id = 'sidebar';
	layout.appendChild(sidebar);
	const detail = renderDetail();
	detail.id = 'detail';
	layout.appendChild(detail);
	root.appendChild(layout);
}

function renderSidebarOnly(): void {
	const old = document.getElementById('sidebar');
	if (!old || !old.parentElement) return;
	const next = renderSidebar();
	next.id = 'sidebar';
	old.parentElement.replaceChild(next, old);
}

function renderDetailOnly(): void {
	const old = document.getElementById('detail');
	if (!old || !old.parentElement) return;
	const next = renderDetail();
	next.id = 'detail';
	old.parentElement.replaceChild(next, old);
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

	// --- Repos ---------------------------------------------------------
	const reposHeader = document.createElement('h3');
	reposHeader.className = 'sidebar-section';
	reposHeader.textContent = 'Repos';
	aside.appendChild(reposHeader);

	if (state.workspaces.length === 0) {
		const empty = document.createElement('p');
		empty.className = 'sidebar-empty';
		empty.textContent = 'No repos. Click "Open Folder…" to add one.';
		aside.appendChild(empty);
	} else {
		const ul = document.createElement('ul');
		ul.className = 'workspaces';
		for (const w of state.workspaces) {
			ul.appendChild(renderRepoItem(w));
		}
		aside.appendChild(ul);
	}

	// --- Containers ----------------------------------------------------
	const linkedCids = new Set(
		Object.values(state.statuses)
			.map((s) => s?.containerId)
			.filter((cid): cid is string => !!cid),
	);
	const cHeader = document.createElement('h3');
	cHeader.className = 'sidebar-section';
	cHeader.textContent = `Containers (${state.containers.length})`;
	aside.appendChild(cHeader);

	if (state.containers.length === 0) {
		const empty = document.createElement('p');
		empty.className = 'sidebar-empty';
		empty.textContent = 'No containers.';
		aside.appendChild(empty);
	} else {
		const ul = document.createElement('ul');
		ul.className = 'workspaces';
		for (const c of state.containers) {
			ul.appendChild(renderContainerItem(c, linkedCids.has(c.containerId)));
		}
		aside.appendChild(ul);
	}

	return aside;
}

function renderRepoItem(w: WorkspaceEntry): HTMLElement {
	const li = document.createElement('li');
	li.dataset.kind = 'repo';
	li.dataset.id = w.id;
	if (state.selected?.kind === 'repo' && state.selected.id === w.id) {
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
	status.textContent = formatRepoStatus(w.id);
	li.append(title, path, status);
	li.addEventListener('click', () => selectRepo(w.id));
	return li;
}

function renderContainerItem(c: ContainerEntry, linked: boolean): HTMLElement {
	const li = document.createElement('li');
	li.dataset.kind = 'container';
	li.dataset.id = c.containerId;
	if (state.selected?.kind === 'container' && state.selected.id === c.containerId) {
		li.classList.add('selected');
	}
	const title = document.createElement('div');
	title.className = 'ws-title';
	title.textContent = c.containerId;
	const meta = document.createElement('div');
	meta.className = 'ws-path';
	meta.textContent = c.imageRef ?? '';
	const status = document.createElement('div');
	status.className = `ws-status state-${c.state}`;
	status.textContent = linked ? `${c.state} · linked` : c.state;
	li.append(title, meta, status);
	li.addEventListener('click', () => selectContainer(c.containerId));
	return li;
}

function formatRepoStatus(workspaceId: string): string {
	const s = state.statuses[workspaceId];
	if (!s) return 'no container';
	const parts: string[] = [s.state];
	if (s.containerId) parts.push(s.containerId);
	return parts.join(' · ');
}

function renderDetail(): HTMLElement {
	const main = document.createElement('section');
	main.className = 'detail';

	if (!state.selected) {
		const empty = document.createElement('p');
		empty.textContent = 'Select a repo or container on the left.';
		main.appendChild(empty);
		return main;
	}

	if (state.selected.kind === 'repo') {
		const ws = state.workspaces.find((w) => w.id === state.selected!.id);
		if (!ws) {
			state.selected = undefined;
			const empty = document.createElement('p');
			empty.textContent = 'Repo no longer present.';
			main.appendChild(empty);
			return main;
		}
		main.appendChild(renderRepoDetailHeader(ws));
		main.appendChild(renderStatusLine(ws.id));
	} else {
		const c = state.containers.find((x) => x.containerId === state.selected!.id);
		if (!c) {
			state.selected = undefined;
			const empty = document.createElement('p');
			empty.textContent = 'Container no longer present.';
			main.appendChild(empty);
			return main;
		}
		main.appendChild(renderContainerDetailHeader(c));
		main.appendChild(renderContainerStatusLine(c));
	}

	main.appendChild(renderLogPane(logKeyForSelection(state.selected)));
	return main;
}

function renderRepoDetailHeader(ws: WorkspaceEntry): HTMLElement {
	const header = document.createElement('div');
	header.className = 'detail-header';
	const title = document.createElement('h2');
	title.textContent = ws.displayName;
	header.appendChild(title);

	const actions = document.createElement('div');
	actions.className = 'actions';
	for (const [label, fn] of [
		['Up', () => repoAction('up', ws.id)],
		['Stop', () => repoAction('stop', ws.id)],
		['Rebuild', () => repoAction('rebuild', ws.id)],
		['Forget', () => forgetRepo(ws.id)],
	] as const) {
		const btn = document.createElement('button');
		btn.textContent = label;
		btn.addEventListener('click', () => void fn());
		actions.appendChild(btn);
	}
	header.appendChild(actions);
	return header;
}

function renderContainerDetailHeader(c: ContainerEntry): HTMLElement {
	const header = document.createElement('div');
	header.className = 'detail-header';
	const title = document.createElement('h2');
	title.textContent = c.containerId;
	header.appendChild(title);

	const actions = document.createElement('div');
	actions.className = 'actions';
	const isRunning = c.state === 'running';
	for (const [label, fn, disabled] of [
		['Start', () => containerAction('start', c.containerId), isRunning],
		['Stop', () => containerAction('stop', c.containerId), !isRunning],
		['Remove', () => containerAction('remove', c.containerId), false],
	] as const) {
		const btn = document.createElement('button');
		btn.textContent = label;
		btn.disabled = disabled;
		btn.addEventListener('click', () => void fn());
		actions.appendChild(btn);
	}
	header.appendChild(actions);
	return header;
}

function renderStatusLine(workspaceId: string): HTMLElement {
	const s = state.statuses[workspaceId];
	const p = document.createElement('p');
	p.className = 'status-line';
	p.id = 'status-line';
	p.textContent = s ? formatStatusLine(s) : 'no container';
	return p;
}

function renderContainerStatusLine(c: ContainerEntry): HTMLElement {
	const p = document.createElement('p');
	p.className = 'status-line';
	p.id = 'status-line';
	p.textContent = `${c.state}${c.imageRef ? ` — ${c.imageRef}` : ''}`;
	return p;
}

function formatStatusLine(s: ContainerStatus): string {
	const parts: string[] = [s.state];
	if (s.containerId) parts.push(s.containerId);
	if (s.error) parts.push(`error: ${s.error}`);
	return parts.join(' \u2014 ');
}

function renderLogPane(logKey: string): HTMLElement {
	const pane = document.createElement('pre');
	pane.className = 'log-pane';
	pane.id = 'log-pane';
	const buf = state.logs.get(logKey);
	if (buf && buf.length > 0) {
		const frag = document.createDocumentFragment();
		for (const entry of buf) {
			frag.appendChild(makeLogLineNode(entry));
		}
		pane.appendChild(frag);
	}
	queueMicrotask(() => {
		pane.scrollTop = pane.scrollHeight;
	});
	return pane;
}

function appendLogToPane(entry: LogLine): void {
	const pane = document.getElementById('log-pane');
	if (!pane) return;
	const nearBottom = pane.scrollHeight - pane.scrollTop - pane.clientHeight < 40;
	pane.appendChild(makeLogLineNode(entry));
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

// Surgical updates that don't tear down the log pane ---------------------

function updateRepoStatusUi(workspaceId: string): void {
	const sidebarStatus = document.querySelector<HTMLElement>(
		`li[data-kind="repo"][data-id="${cssEscape(workspaceId)}"] .ws-status`,
	);
	if (sidebarStatus) {
		sidebarStatus.textContent = formatRepoStatus(workspaceId);
	}
	if (state.selected?.kind === 'repo' && state.selected.id === workspaceId) {
		const line = document.getElementById('status-line');
		const s = state.statuses[workspaceId];
		if (line) line.textContent = s ? formatStatusLine(s) : 'no container';
	}
}

function updateContainerDetail(): void {
	if (state.selected?.kind !== 'container') return;
	const cid = state.selected.id;
	const c = state.containers.find((x) => x.containerId === cid);
	if (!c) return;
	const line = document.getElementById('status-line');
	if (line) line.textContent = `${c.state}${c.imageRef ? ` — ${c.imageRef}` : ''}`;
}

function cssEscape(s: string): string {
	return s.replace(/["\\]/g, '\\$&');
}

// =============================================================================
// Selection
// =============================================================================

function selectRepo(id: string): void {
	state.selected = { kind: 'repo', id };
	renderSidebarOnly();
	renderDetailOnly();
	void refreshRepoStatus(id);
	void loadConfigForWorkspace(id);
}

function selectContainer(id: string): void {
	state.selected = { kind: 'container', id };
	renderSidebarOnly();
	renderDetailOnly();
}

async function refreshRepoStatus(id: string): Promise<void> {
	try {
		state.statuses[id] = await bridge.container_status(id);
	} catch (err) {
		state.statuses[id] = {
			workspaceId: id,
			state: 'error',
			error: errMsg(err),
		};
	}
	updateRepoStatusUi(id);
}

async function loadConfigForWorkspace(id: string): Promise<void> {
	const ws = state.workspaces.find((w) => w.id === id);
	if (!ws) return;
	const key = logKeyForWorkspace(id);
	try {
		const eng = await engine();
		const result = await eng.loadAndSubmitDevContainerConfig(
			eng.tauriFileHost(),
			id,
			ws.path,
		);
		if (!result) {
			logLocal(key, 'system', `No .devcontainer/devcontainer.json found in ${ws.path}`);
		}
	} catch (err) {
		logLocal(key, 'stderr', `Failed to load devcontainer.json: ${errMsg(err)}`);
	}
}

// =============================================================================
// Actions
// =============================================================================

async function onOpenFolder(): Promise<void> {
	const picked = await openDialog({ directory: true, multiple: false });
	if (typeof picked !== 'string') return;
	try {
		const ws = await bridge.add_workspace(picked);
		state.workspaces.push(ws);
		state.selected = { kind: 'repo', id: ws.id };
	} catch (err) {
		const msg = errMsg(err);
		logLocal(`repo:${picked}`, 'stderr', `Failed to add workspace: ${msg}`);
		state.statuses[picked] = { workspaceId: picked, state: 'error', error: msg };
	}
	renderSidebarOnly();
	renderDetailOnly();
	if (state.selected?.kind === 'repo') {
		void loadConfigForWorkspace(state.selected.id);
	}
}

type RepoAction = 'up' | 'stop' | 'rebuild';

async function repoAction(kind: RepoAction, workspaceId: string): Promise<void> {
	const key = logKeyForWorkspace(workspaceId);
	try {
		const fn = {
			up: bridge.container_up,
			stop: bridge.container_stop,
			rebuild: bridge.container_rebuild,
		}[kind];
		const status = await fn(workspaceId);
		state.statuses[workspaceId] = status;
	} catch (err) {
		const msg = errMsg(err);
		state.statuses[workspaceId] = {
			workspaceId,
			state: 'error',
			error: msg,
		};
		logLocal(key, 'stderr', `${kind} failed: ${msg}`);
	}
	updateRepoStatusUi(workspaceId);
	void refreshContainers();
}

async function forgetRepo(workspaceId: string): Promise<void> {
	const key = logKeyForWorkspace(workspaceId);
	try {
		await bridge.remove_workspace(workspaceId);
	} catch (err) {
		logLocal(key, 'stderr', `Forget failed: ${errMsg(err)}`);
		return;
	}
	state.workspaces = state.workspaces.filter((w) => w.id !== workspaceId);
	delete state.statuses[workspaceId];
	state.logs.delete(key);
	if (state.selected?.kind === 'repo' && state.selected.id === workspaceId) {
		state.selected = undefined;
	}
	renderSidebarOnly();
	renderDetailOnly();
}

type ContainerActionKind = 'start' | 'stop' | 'remove';

async function containerAction(kind: ContainerActionKind, containerId: string): Promise<void> {
	const key = logKeyForContainer(containerId);
	logLocal(key, 'system', `${kind} ${containerId}`);
	try {
		if (kind === 'start') await bridge.container_start_by_id(containerId);
		else if (kind === 'stop') await bridge.container_stop_by_id(containerId);
		else await bridge.container_remove_by_id(containerId, true);
	} catch (err) {
		logLocal(key, 'stderr', `${kind} failed: ${errMsg(err)}`);
	}
	await refreshContainers();
}

// =============================================================================
// Helpers
// =============================================================================

/** Coerce any thrown value into a useful message string. Tauri rejects
 *  with the plain `String` returned from `Result<_, String>`, so
 *  `(err as Error).message` is `undefined` in that path. */
function errMsg(err: unknown): string {
	if (err == null) return 'unknown error';
	if (typeof err === 'string') return err;
	if (err instanceof Error) return err.message;
	if (typeof err === 'object' && 'message' in err) {
		const m = (err as { message: unknown }).message;
		if (typeof m === 'string') return m;
	}
	try {
		return JSON.stringify(err);
	} catch {
		return String(err);
	}
}

void bootstrap();

/*---------------------------------------------------------------------------------------------
 *  Devcontainers.app — dashboard entry point.
 *
 *  This is a deliberately minimal scaffold; the MVP UI (dashboard cards,
 *  workspace detail, terminal panel, log streaming) is step 8 of the
 *  conversion roadmap.
 *--------------------------------------------------------------------------------------------*/

import { bridge } from './lib/bridge';

async function bootstrap(): Promise<void> {
	const root = document.getElementById('app');
	if (!root) {
		return;
	}
	root.textContent = 'Loading workspaces…';
	try {
		const [workspaces, runtimes] = await Promise.all([
			bridge.list_workspaces(),
			bridge.list_runtimes(),
		]);
		render(root, workspaces, runtimes);
	} catch (err) {
		root.textContent = `Failed to load: ${(err as Error).message}`;
	}
}

function render(root: HTMLElement, workspaces: Awaited<ReturnType<typeof bridge.list_workspaces>>, runtimes: Awaited<ReturnType<typeof bridge.list_runtimes>>): void {
	root.replaceChildren();

	const heading = document.createElement('h1');
	heading.textContent = 'Devcontainers.app';
	root.appendChild(heading);

	const runtimeList = document.createElement('ul');
	runtimeList.className = 'runtimes';
	for (const r of runtimes) {
		const li = document.createElement('li');
		li.textContent = `${r.id}: ${r.available ? (r.version ?? 'available') : (r.reason ?? 'unavailable')}`;
		runtimeList.appendChild(li);
	}
	root.appendChild(runtimeList);

	if (workspaces.length === 0) {
		const empty = document.createElement('p');
		empty.textContent = 'No workspaces yet. Use File → Open Folder… to add one.';
		root.appendChild(empty);
		return;
	}

	const ul = document.createElement('ul');
	ul.className = 'workspaces';
	for (const w of workspaces) {
		const li = document.createElement('li');
		li.textContent = `${w.displayName} — ${w.path}`;
		ul.appendChild(li);
	}
	root.appendChild(ul);
}

void bootstrap();

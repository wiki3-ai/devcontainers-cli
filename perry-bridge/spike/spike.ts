// spike.ts — Perry capability spike.
//
// Purpose: prove that the Perry stdlib covers everything the slice actually
// uses, before we invest in the full bundle. Run with:
//
//   perry compile perry-bridge/spike/spike.ts -o /tmp/perry-spike
//   /tmp/perry-spike
//
// Expected output: a single JSON object with `ok: true` and a `report`
// listing each capability and whether it succeeded.
//
// Anything that fails here is a re-plan trigger documented in
// ../api-audit.md.

import * as path from 'path';
import * as jsonc from 'jsonc-parser';
import { URI } from 'vscode-uri';

interface CheckResult {
	name: string;
	ok: boolean;
	detail?: string;
}

const checks: CheckResult[] = [];

function check(name: string, fn: () => void | Promise<void>): Promise<void> {
	const exec = async () => {
		try {
			await fn();
			checks.push({ name, ok: true });
		} catch (err) {
			checks.push({ name, ok: false, detail: String((err as Error).message ?? err) });
		}
	};
	return exec();
}

async function main(): Promise<void> {
	await check('path.posix.join', () => {
		if (path.posix.join('/a', 'b', 'c') !== '/a/b/c') {
			throw new Error('mismatch');
		}
	});
	await check('path.win32.join', () => {
		if (path.win32.join('C:\\a', 'b') !== 'C:\\a\\b') {
			throw new Error('mismatch');
		}
	});
	await check('JSON.parse / stringify', () => {
		const o = JSON.parse('{"a":1}') as { a: number };
		if (JSON.stringify(o) !== '{"a":1}') {
			throw new Error('mismatch');
		}
	});
	await check('jsonc-parser.parse with comments', () => {
		const r = jsonc.parse('{\n// hi\n"a": 1\n}') as { a: number };
		if (r.a !== 1) {
			throw new Error('mismatch');
		}
	});
	await check('vscode-uri URI.file + uri.path', () => {
		const u = URI.file('/tmp/x');
		if (u.path !== '/tmp/x') {
			throw new Error(`unexpected path: ${u.path}`);
		}
	});
	await check('Buffer base64 round-trip', () => {
		const round = Buffer.from('hello', 'utf8').toString('base64');
		const back = Buffer.from(round, 'base64').toString('utf8');
		if (back !== 'hello') {
			throw new Error('mismatch');
		}
	});
	await check('process.argv reachable', () => {
		if (!Array.isArray(process.argv)) {
			throw new Error('argv missing');
		}
	});
	await check('process.stdout.write', () => {
		// We rely on this to send NDJSON in entry.ts; a missing impl is fatal.
		if (typeof process.stdout.write !== 'function') {
			throw new Error('stdout.write missing');
		}
	});

	const ok = checks.every(c => c.ok);
	process.stdout.write(JSON.stringify({ ok, report: checks }, null, 2) + '\n');
	process.exit(ok ? 0 : 1);
}

main().catch(err => {
	process.stdout.write(JSON.stringify({ ok: false, error: String(err) }) + '\n');
	process.exit(1);
});

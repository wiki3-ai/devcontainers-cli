// Build script for the devcontainer engine ES module that is loaded by the
// Tauri WebView at app runtime. Run via `deno task build`.
//
// The output is a single ES module at `dist/devcontainer-engine.js` that
// re-exports the spec slice (FileHost-only). Node built-ins are *not*
// permitted in the bundle; this is enforced both via the import map in
// `deno.json` (which redirects bare `path`/`crypto` to `node:` specifiers
// that Deno's node-compat layer inlines) and via
// `scripts/check-no-node-builtins.ts`, which CI runs after the build.

import * as esbuild from 'npm:esbuild@^0.27.3';
import { polyfillNode } from 'npm:esbuild-plugin-polyfill-node@^0.3.0';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = dirname(dirname(fileURLToPath(import.meta.url)));
const entry = join(repoRoot, 'frontend', 'src', 'devcontainer-engine', 'index.ts');
const outFile = join(repoRoot, 'dist', 'devcontainer-engine.js');

await esbuild.build({
	entryPoints: [entry],
	bundle: true,
	format: 'esm',
	platform: 'browser',
	target: ['es2022'],
	outfile: outFile,
	sourcemap: true,
	logLevel: 'info',
	plugins: [
		// Polyfill the Node built-ins the spec slice still relies on
		// (`node:path`, `node:crypto`, `node:url`). The deny-list check in
		// `scripts/check-no-node-builtins.ts` enforces that *only* this
		// curated set is permitted; bare `fs`/`child_process`/etc. fail CI.
		polyfillNode({
			polyfills: {
				path: true,
				crypto: true,
				url: true,
				fs: false,
				child_process: false,
				os: 'empty',
				tty: false,
				net: false,
			},
		}) as unknown as esbuild.Plugin,
	],
	// The host-only Node modules that the spec slice never touches stay
	// external; if a future change accidentally pulls them in, esbuild
	// errors out and the deny-list check confirms it.
	external: ['fs', 'fs/promises', 'child_process', 'tar', 'ncp'],
});

await esbuild.stop();
console.log(`✔ wrote ${outFile}`);

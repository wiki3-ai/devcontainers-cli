// Sanity-check that the bundled engine does not pull in Node built-ins. The
// Deno build maps `path`/`crypto` to `node:` specifiers that Deno inlines as
// pure-JS implementations, but a stray `import * as fs from 'fs'` in any
// future change must fail CI rather than break in the WebView at runtime.
//
// Usage: `deno run --allow-read scripts/check-no-node-builtins.ts <bundle>`

const target = Deno.args[0];
if (!target) {
	console.error('usage: check-no-node-builtins.ts <bundle.js>');
	Deno.exit(2);
}

const banned = [
	// Bare CommonJS-style requires
	/\brequire\(\s*["'](fs|fs\/promises|child_process|os|tty|net|http|https|stream|zlib|tar|ncp|node-pty|proxy-agent|follow-redirects|yargs)["']\s*\)/g,
	// ESM imports from bare specifiers (the bundler should have resolved or externalised these)
	/\bfrom\s+["'](fs|fs\/promises|child_process|os|tty|net|http|https|stream|zlib|tar|ncp|node-pty|proxy-agent|follow-redirects|yargs)["']/g,
	// `node:` prefixed imports of side-effecting modules
	/\bfrom\s+["']node:(fs|fs\/promises|child_process|os|tty|net|http|https|stream|zlib)["']/g,
];

const text = await Deno.readTextFile(target);
let bad = 0;
for (const pat of banned) {
	for (const m of text.matchAll(pat)) {
		console.error(`✗ banned import in bundle: ${m[0]}`);
		bad++;
	}
}
if (bad > 0) {
	console.error(`Found ${bad} banned imports in ${target}`);
	Deno.exit(1);
}
console.log(`✔ ${target} contains no banned Node built-in imports`);

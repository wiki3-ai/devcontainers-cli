import { defineConfig } from 'vite';

// Vite config for the Tauri WebView. The Rust app loads the built bundle from
// `frontend/dist/`. Port 1420 mirrors the Tauri 2 starter convention so
// `tauri dev` can attach to the Vite dev server.
export default defineConfig({
	clearScreen: false,
	server: {
		port: 1420,
		strictPort: true,
	},
	build: {
		target: 'es2022',
		outDir: 'dist',
		emptyOutDir: true,
		sourcemap: true,
		rollupOptions: {
			// `/devcontainer-engine.js` is the deno-built spec slice bundle
			// served from `frontend/public/`. We mark it external so Rollup
			// keeps the runtime URL intact and does not try to resolve and
			// inline a module that depends on Node built-ins.
			external: ['/devcontainer-engine.js'],
		},
	},
});

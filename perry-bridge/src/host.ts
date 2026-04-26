// host.ts
//
// FileHost implementation that talks to the Rust side over newline-delimited
// JSON on stdin / stdout. See ../PROTOCOL.md for the wire contract.
//
// This module is the only place in the slice that interacts with the
// "outside world". Everything else (jsonc parsing, variable substitution,
// updateFromOldProperties) runs in pure TS and is Perry-safe as-is.

import * as path from 'path';

// We mirror the FileHost interface from src/spec-utils/pfs.ts here rather than
// importing it, so we don't pull pfs.ts (and its `ncp`/`util.promisify`
// dependencies) into the Perry bundle. Keep the shape in sync; the audit
// (api-audit.md) records this as deliberate.
export enum FileTypeBitmask {
	Unknown = 0,
	File = 1,
	Directory = 2,
	SymbolicLink = 64,
}

export interface FileHost {
	platform: NodeJS.Platform;
	path: typeof path.posix | typeof path.win32;
	isFile(filepath: string): Promise<boolean>;
	readFile(filepath: string): Promise<Buffer>;
	writeFile(filepath: string, content: Buffer): Promise<void>;
	readDir(dirpath: string): Promise<string[]>;
	readDirWithTypes?(dirpath: string): Promise<[string, FileTypeBitmask][]>;
	mkdirp(dirpath: string): Promise<void>;
	toCommonURI(filePath: string): Promise<undefined>;
}

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

type HostReply =
	| { kind: 'host-reply'; id: number; ok: true; value: any }
	| { kind: 'host-reply'; id: number; ok: false; error: { code?: string; message: string } };

export interface HostChannel {
	send(line: string): void;
	recv(): Promise<string>;
}

let nextId = 1;
const pending = new Map<number, (reply: HostReply) => void>();
let channel: HostChannel | undefined;
let readerStarted = false;

export function setChannel(ch: HostChannel): void {
	channel = ch;
	if (!readerStarted) {
		readerStarted = true;
		void readerLoop();
	}
}

async function readerLoop(): Promise<void> {
	if (!channel) {
		return;
	}
	while (true) {
		const line = await channel.recv();
		if (!line) {
			return;
		}
		let msg: HostReply;
		try {
			msg = JSON.parse(line) as HostReply;
		} catch {
			// Ignore malformed lines; the Rust side is the source of truth.
			continue;
		}
		if (msg.kind !== 'host-reply') {
			continue;
		}
		const cb = pending.get(msg.id);
		if (cb) {
			pending.delete(msg.id);
			cb(msg);
		}
	}
}

function call<T>(op: string, args: Record<string, unknown>): Promise<T> {
	if (!channel) {
		return Promise.reject(new Error('host channel not initialised'));
	}
	const id = nextId++;
	return new Promise<T>((resolve, reject) => {
		pending.set(id, reply => {
			if (reply.ok) {
				resolve(reply.value as T);
			} else {
				const err: NodeJS.ErrnoException = new Error(reply.error.message);
				err.code = reply.error.code;
				reject(err);
			}
		});
		channel!.send(JSON.stringify({ kind: 'host', id, op, args }));
	});
}

// ---------------------------------------------------------------------------
// Base64 (Buffer-free path; Perry has Buffer but we keep this compatible)
// ---------------------------------------------------------------------------

function bytesFromBase64(b64: string): Buffer {
	return Buffer.from(b64, 'base64');
}

function bytesToBase64(buf: Buffer): string {
	return buf.toString('base64');
}

// ---------------------------------------------------------------------------
// PerryFileHost
// ---------------------------------------------------------------------------

interface StatReply {
	kind: 'file' | 'dir' | 'other' | 'missing';
	size?: number;
}

interface ReadDirReply {
	entries: { name: string; kind: 'file' | 'dir' | 'other' }[];
}

export function createPerryFileHost(platform: NodeJS.Platform): FileHost {
	const p = platform === 'win32' ? path.win32 : path.posix;
	return {
		platform,
		path: p,
		async isFile(filepath: string): Promise<boolean> {
			const r = await call<StatReply>('fs.stat', { path: filepath });
			return r.kind === 'file';
		},
		async readFile(filepath: string): Promise<Buffer> {
			const r = await call<{ bytesBase64: string }>('fs.readFile', { path: filepath });
			return bytesFromBase64(r.bytesBase64);
		},
		async writeFile(filepath: string, content: Buffer): Promise<void> {
			await call<{}>('fs.writeFile', { path: filepath, bytesBase64: bytesToBase64(content) });
		},
		async readDir(dirpath: string): Promise<string[]> {
			const r = await call<ReadDirReply>('fs.readDir', { path: dirpath });
			return r.entries.map(e => e.name);
		},
		async readDirWithTypes(dirpath: string): Promise<[string, FileTypeBitmask][]> {
			const r = await call<ReadDirReply>('fs.readDir', { path: dirpath });
			return r.entries.map(e => {
				let bits: FileTypeBitmask = FileTypeBitmask.Unknown;
				if (e.kind === 'file') {
					bits = FileTypeBitmask.File;
				} else if (e.kind === 'dir') {
					bits = FileTypeBitmask.Directory;
				}
				return [e.name, bits] as [string, FileTypeBitmask];
			});
		},
		async mkdirp(_dirpath: string): Promise<void> {
			// Not needed for the load-config slice. If a future command
			// needs it, add fs.mkdirp to PROTOCOL.md and wire it through.
			throw new Error('mkdirp is not implemented in perry-bridge');
		},
		async toCommonURI(_filePath: string): Promise<undefined> {
			return undefined;
		},
	};
}

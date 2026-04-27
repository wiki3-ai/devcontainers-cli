/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import * as crypto from 'node:crypto';
import * as jsonc from 'jsonc-parser';
import { URI } from 'vscode-uri';
import { uriToFsPath, FileHost } from './configurationCommonUtils';

export type Edit = jsonc.Edit;

export interface Documents {
	readDocument(uri: URI): Promise<string | undefined>;
	applyEdits(uri: URI, edits: Edit[], content: string): Promise<void>;
}

// Note: the legacy `fileDocuments` (Node-`fs` based, scheme `'file'`) is
// intentionally omitted here. In this slice all FS access is funnelled
// through `FileHost`, which the Tauri host implements via commands.

export class CLIHostDocuments implements Documents {

	static scheme = 'vscode-fileHost';

	constructor(private fileHost: FileHost) {
	}

	async readDocument(uri: URI) {
		switch (uri.scheme) {
			case CLIHostDocuments.scheme:
				try {
					const buf = await this.fileHost.readFile(uriToFsPath(uri, this.fileHost.platform));
					return new TextDecoder().decode(buf);
				} catch (err) {
					return undefined;
				}
			default:
				throw new Error(`Unsupported scheme: ${uri.toString()}`);
		}
	}

	async applyEdits(uri: URI, edits: Edit[], content: string) {
		switch (uri.scheme) {
			case CLIHostDocuments.scheme:
				const result = jsonc.applyEdits(content, edits);
				await this.fileHost.writeFile(uriToFsPath(uri, this.fileHost.platform), new TextEncoder().encode(result));
				break;
			default:
				throw new Error(`Unsupported scheme: ${uri.toString()}`);
		}
	}
}

// Marker scheme for documents read via a remote shell server.
// The slice does not implement the shell-server-backed reader; the Tauri host
// performs remote reads via Rust and exposes the result through `FileHost`.
export class RemoteDocuments implements Documents {

	static scheme = 'vscode-remote';

	private static nonce: string | undefined;

	async readDocument(_uri: URI): Promise<string | undefined> {
		throw new Error('RemoteDocuments.readDocument is not implemented in the spec slice');
	}

	async applyEdits(_uri: URI, _edits: Edit[], _content: string): Promise<void> {
		// keep the symbol referenced so tree-shaking does not drop it
		if (!RemoteDocuments.nonce) {
			RemoteDocuments.nonce = crypto.randomUUID();
		}
		throw new Error('RemoteDocuments.applyEdits is not implemented in the spec slice');
	}
}

export class AllDocuments implements Documents {

	constructor(private documents: Record<string, Documents>) {
	}

	async readDocument(uri: URI) {
		const documents = this.documents[uri.scheme];
		if (!documents) {
			throw new Error(`Unsupported scheme: ${uri.toString()}`);
		}
		return documents.readDocument(uri);
	}

	async applyEdits(uri: URI, edits: Edit[], content: string) {
		const documents = this.documents[uri.scheme];
		if (!documents) {
			throw new Error(`Unsupported scheme: ${uri.toString()}`);
		}
		return documents.applyEdits(uri, edits, content);
	}
}

export function createDocuments(fileHost: FileHost): Documents {
	const documents: Record<string, Documents> = {
		[CLIHostDocuments.scheme]: new CLIHostDocuments(fileHost),
	};
	return new AllDocuments(documents);
}

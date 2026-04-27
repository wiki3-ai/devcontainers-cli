/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

// Slim error type used by the spec slice. The legacy `src/spec-common/errors.ts`
// pulls in `injectHeadless` (which drags in resolver/Docker/Compose machinery);
// the slice only needs a plain `ContainerError`.

interface ContainerErrorInfo {
	description: string;
	originalError?: any;
	data?: Record<string, unknown>;
}

export class ContainerError extends Error {
	description!: string;
	originalError?: any;
	data: Record<string, unknown> = {};

	constructor(info: ContainerErrorInfo) {
		super((info.originalError && info.originalError.message) || info.description);
		Object.assign(this, info);
		if (info.originalError?.stack) {
			this.stack = info.originalError.stack;
		}
	}
}

// re-exports.ts
//
// Single import surface for the slice. Anything imported here gets pulled
// into the Perry bundle; anything not imported here does not. Keep this list
// minimal and audited (see ../api-audit.md).

export {
	DevContainerConfig,
	DevContainerFromImageConfig,
	DevContainerFromDockerfileConfig,
	DevContainerFromDockerComposeConfig,
	updateFromOldProperties,
} from '../../src/spec-configuration/configuration';

export {
	uriToFsPath,
	parentURI,
	getWellKnownDevContainerPaths,
} from '../../src/spec-configuration/configurationCommonUtils';

export { fileDocuments } from '../../src/spec-configuration/editableFiles';

export {
	substitute,
	SubstitutionContext,
} from '../../src/spec-common/variableSubstitution';

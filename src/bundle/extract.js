// Shared by both prologues (pibake splices it in at `// pibake:extract`): the stage files a pibake
// executable carries as `files/<path>` SEA assets, extracted into `root`. Each launch extracts
// into its own temporary directory and renames it into place; a launch that loses the race uses
// the winner's. A directory without the marker was not made here.
function __pibake_extract(base, root, hash) {
	const marker = ".pibake-extracted";
	if (__pibake_fs.existsSync(__pibake_join(root, marker))) return;
	__pibake_fs.mkdirSync(base, { recursive: true });
	const tmp = __pibake_fs.mkdtempSync(__pibake_join(base, `.tmp-${hash}-`));
	try {
		for (const key of __pibake_sea.getAssetKeys()) {
			if (!key.startsWith("files/")) continue;
			const file = __pibake_join(tmp, key.slice("files/".length));
			__pibake_fs.mkdirSync(__pibake_dirname(file), { recursive: true });
			__pibake_fs.writeFileSync(file, new Uint8Array(__pibake_sea.getRawAsset(key)));
		}
		__pibake_fs.writeFileSync(__pibake_join(tmp, marker), `${hash}\n`);
		try {
			__pibake_fs.renameSync(tmp, root);
		} catch (e) {
			if (!__pibake_fs.existsSync(__pibake_join(root, marker))) {
				throw new Error(`${root} exists without ${marker}; remove it`, { cause: e });
			}
		}
	} finally {
		__pibake_fs.rmSync(tmp, { recursive: true, force: true });
	}
}

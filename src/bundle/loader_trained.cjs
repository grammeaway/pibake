// pibake `sea_trained` executable main. Compiles the bundle (a function expression, see
// src/bundle.rs) with the code cache trained at build time (src/train.rs), then runs it. V8
// compiles from source what the cache does not hold, or all of it if it rejects the cache.
// PIBAKE_CACHE_CHECK=1 exits after compiling: 3 if the cache was rejected. See src/sea.rs.
const sea = require("node:sea");
const vm = require("node:vm");
const script = new vm.Script(sea.getAsset("bundle.cjs", "utf8"), {
	filename: __filename,
	cachedData: new Uint8Array(sea.getRawAsset("cache.bin")),
});
if (process.env.PIBAKE_CACHE_CHECK === "1") process.exit(script.cachedDataRejected ? 3 : 0);
script.runInThisContext()(exports, require, module, __filename, __dirname);

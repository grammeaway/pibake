// pibake training run (src/train.rs): `node train.cjs <cache> <main.cjs> <pi args>` runs the
// `sea_trained` bundle as pi.cjs does, compiled with the code cache <cache> once it exists, and
// writes back to <cache> the cache of everything compiled by exit.
"use strict";
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const [cache, file] = process.argv.splice(2, 2);
process.argv[1] = file;
const cachedData = fs.existsSync(cache) ? fs.readFileSync(cache) : undefined;
const script = new vm.Script(fs.readFileSync(file, "utf8"), { filename: file, cachedData });
if (script.cachedDataRejected) throw new Error(`pibake: V8 rejected the code cache ${cache}`);
process.on("exit", () => fs.writeFileSync(cache, script.createCachedData()));
script.runInThisContext()(exports, require, module, file, path.dirname(file));

// pibake `sea_trained` bundle launcher, for plain `node`: compiles main.cjs, a function
// expression (see src/bundle.rs), and runs it, as the executable's loader does.
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const { Script } = require("node:vm");
const file = join(__dirname, "main.cjs");
new Script(readFileSync(file, "utf8"), { filename: file }).runInThisContext()(exports, require, module, file, __dirname);

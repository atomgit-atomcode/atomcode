#!/usr/bin/env node
'use strict';
// Explicit host action only; never registered as an MCP tool.
const path = require('node:path');
const { prepareOutput } = require('./backend.cjs');
const { stageFile } = require('./upload-store.cjs');
async function main(args) {
  if (args.length !== 4 || args[0] !== '--output-dir' || args[2] !== '--file' || !path.isAbsolute(args[1]) || !path.isAbsolute(args[3]))
    throw new Error('Expected --output-dir <owned absolute directory> --file <explicit absolute file>');
  prepareOutput(args[1]);
  return stageFile(path.join(args[1], 'uploads'), args[3]);
}
if (require.main === module) main(process.argv.slice(2)).then(value => {
  process.stdout.write(JSON.stringify(value) + '\n');
}).catch(() => {
  process.stderr.write('Upload staging failed; check the explicit regular file, size and owned output directory.\n');
  process.exitCode = 1;
});
module.exports = { main };

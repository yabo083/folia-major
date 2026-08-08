// scripts/generate-update-manifest.mjs
// M10：由 CI 调用，为某个发布通道生成 Tauri v2 updater 端点 manifest。
// 用法：
//   node scripts/generate-update-manifest.mjs \
//     --channel <realeco|limo|cielo> \
//     --version <A.B.C[-pre]> \
//     --setup-exe <Folia_<version>_x64-setup.exe> \
//     --signature-file <同名的 .sig 文件> \
//     --base-url <https://host/path> \
//     [--notes-file <release notes>] \
//     [--out <manifest.json>]
// 输出形如：{ version, notes?, pub_date, platforms: { "windows-x86_64": { signature, url } } }
// 通道门禁由 shared/updateChannels.mjs 的 buildChannelManifest 强制（版本不属于通道即抛错）。

import { readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { buildChannelManifest, channelManifestName } from '../shared/updateChannels.mjs';

const currentDir = path.dirname(fileURLToPath(import.meta.url));

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 1) {
    const flag = argv[i];
    if (flag.startsWith('--')) {
      args[flag.slice(2)] = argv[i + 1];
      i += 1;
    }
  }
  return args;
}

const args = parseArgs(process.argv.slice(2));
const required = ['channel', 'version', 'setup-exe', 'signature-file', 'base-url'];
const missing = required.filter((key) => !args[key]);
if (missing.length > 0) {
  console.error(`Missing required arguments: ${missing.join(', ')}`);
  process.exit(2);
}

const channel = args.channel.trim().toLowerCase();
const manifestName = channelManifestName(channel);
if (!manifestName) {
  console.error(`Unknown or update-disabled channel: ${args.channel}`);
  process.exit(2);
}

const version = args.version.trim().replace(/^v/i, '');
const setupExePath = path.resolve(currentDir, '..', args['setup-exe']);
const signaturePath = path.resolve(currentDir, '..', args['signature-file']);

let signature;
try {
  signature = readFileSync(signaturePath, 'utf8').trim();
} catch (error) {
  console.error(`Cannot read signature file ${signaturePath}: ${error.message}`);
  process.exit(2);
}

let notes = null;
if (args['notes-file']) {
  const notesPath = path.resolve(currentDir, '..', args['notes-file']);
  try {
    notes = readFileSync(notesPath, 'utf8').trim();
  } catch (error) {
    console.error(`Cannot read notes file ${notesPath}: ${error.message}`);
    process.exit(2);
  }
}

const artifactFileName = path.basename(setupExePath);
const baseUrl = args['base-url'].replace(/\/+$/, '');
const url = `${baseUrl}/${artifactFileName}`;

let manifest;
try {
  manifest = buildChannelManifest({ channel, version, signature, url, notes });
} catch (error) {
  console.error(`Refusing to generate manifest: ${error.message}`);
  process.exit(1);
}

const output = args.out ? path.resolve(currentDir, '..', args.out) : null;
const payload = `${JSON.stringify(manifest, null, 2)}\n`;
if (output) {
  writeFileSync(output, payload, 'utf8');
  console.log(`Wrote ${manifestName} (channel ${channel}, version ${version}) to ${output}`);
} else {
  process.stdout.write(payload);
}

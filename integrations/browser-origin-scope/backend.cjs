#!/usr/bin/env node
'use strict';

const fs = require('node:fs');
const { createHash } = require('node:crypto');
const path = require('node:path');
const { createRequire } = require('node:module');
const { CAPABILITY, POLICY_ENV, parsePolicy, policyDetails, createProxy, PolicyTransport } = require('./policy.cjs');

const { ControlledDownloads } = require('./transfers.cjs');

const MCP_VERSION = '0.0.85';
const CORE_VERSION = '1.66.0-alpha-2026-11-09';

function loadBackend(root) {
  if (!path.isAbsolute(root)) throw new Error('backend-root must be the absolute installed @playwright/mcp directory');
  if (Number(process.versions.node.split('.')[0]) < 20) throw new Error('This pinned Playwright runtime requires Node 20+');
  const requireBackend = createRequire(path.join(root, 'package.json'));
  if (requireBackend('./package.json').name !== '@playwright/mcp' || requireBackend('./package.json').version !== MCP_VERSION ||
      requireBackend('playwright-core/package.json').version !== CORE_VERSION)
    throw new Error('Unsupported external browser backend versions');
  const { createConnection } = requireBackend('./index.js');
  const { chromium } = requireBackend('playwright-core');
  const { StdioServerTransport } = requireBackend('playwright-core/lib/utilsBundle');
  const sourceHashes = {};
  for (const name of ['./package.json', './index.js', 'playwright-core/lib/coreBundle', 'playwright-core/lib/utilsBundle'])
    sourceHashes[name] = createHash('sha256').update(fs.readFileSync(requireBackend.resolve(name))).digest('hex');
  return { createConnection, chromium, StdioServerTransport, sourceHashes };
}

function prepareOutput(outputDir) {
  if (!path.isAbsolute(outputDir) || path.resolve(outputDir) === path.parse(outputDir).root)
    throw new Error('output-dir must be an owned absolute directory, never a filesystem root');
  try {
    const stat = fs.lstatSync(outputDir);
    if (!stat.isDirectory() || (process.getuid && stat.uid !== process.getuid()))
      throw new Error('output-dir must be a real directory owned by this user');
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
    fs.mkdirSync(outputDir, { recursive: true, mode: 0o700 });
  }
  const marker = path.join(outputDir, '.browser-origin-scope-v1');
  const identity = 'atomcode/browser-origin-scope artifacts v1\n';
  try {
    if (!fs.lstatSync(marker).isFile() || fs.readFileSync(marker, 'utf8') !== identity)
      throw new Error('unrecognized artifact ownership marker');
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
    if (fs.readdirSync(outputDir).length)
      throw new Error('refusing an existing nonempty directory without artifact ownership');
    fs.writeFileSync(marker, identity, { flag: 'wx', mode: 0o600 });
  }
  if (process.platform !== 'win32') fs.chmodSync(outputDir, 0o700);
  // Retention/eviction is confined below this marker, never an arbitrary
  // existing directory or the ownership marker itself.
  const files = path.join(outputDir, 'files');
  try { if (!fs.lstatSync(files).isDirectory()) throw new Error('artifact files must be a real directory'); }
  catch (error) { if (error.code !== 'ENOENT') throw error; fs.mkdirSync(files, { mode: 0o700 }); }
  if (process.platform !== 'win32') fs.chmodSync(files, 0o700);
  return files;
}

// Tests may supply an owned launch function to exercise the exact context
// factory; production always launches the installed Chrome through a pipe.
function ownedBrowser(chromium, origins, headed) {
  let current;
  const contextOpenings = new WeakMap(), disposedOpenings = new WeakMap();
  let closing = false;
  let cleanup = Promise.resolve();
  async function get() {
    if (closing) throw new Error('Browser backend is closing');
    if (!current) {
      const previousCleanup = cleanup;
      const opening = (async () => {
        await previousCleanup;
        if (closing) throw new Error("Browser backend is closing");
        const proxy = await createProxy(origins);
        let browser;
        try {
          browser = await chromium.launch({ channel: 'chrome', headless: !headed,
            proxy: { server: proxy.url, bypass: '<-loopback>' },
            args: ['--disable-quic', '--disable-background-networking', '--dns-prefetch-disable',
              '--force-webrtc-ip-handling-policy=disable_non_proxied_udp'] });
          const context = await browser.newContext({ acceptDownloads: false, serviceWorkers: 'block', permissions: [] });
          contextOpenings.set(context, opening);
          return { browser, context, proxy };
        } catch (error) {
          await browser?.close().catch(() => {});
          await proxy.close();
          throw error;
        }
      })();
      current = opening;
      opening.catch(() => { if (current === opening) current = undefined; });
    }
    return (await current).context;
  }
  function close(permanent = false) {
    if (permanent) closing = true;
    const old = current;
    current = undefined;
    const previousCleanup = cleanup;
    cleanup = (async () => {
      const [previous, opened] = await Promise.allSettled([previousCleanup, old]);
      const owned = opened.status === 'fulfilled' ? opened.value : undefined;
      if (owned) {
        try { await owned.browser.close(); } finally { await owned.proxy.close(); }
      }
      if (previous.status === 'rejected') throw previous.reason;
    })();
    if (old) disposedOpenings.set(old, cleanup);
    return cleanup;
  }

  function closeCaptured(context) {
    const opening = contextOpenings.get(context);
    if (!opening) return Promise.reject(new Error('Unknown owned context'));
    return opening === current ? close() : disposedOpenings.get(opening) || Promise.resolve();
  }

  async function existing() {
    const opening = current;
    if (!opening || closing) throw new Error('No existing owned browser context');
    const owned = await opening;
    if (current !== opening || closing) throw new Error('Owned context was replaced or closed');
    return owned.context;
  }
  return { get, close, closeCaptured, existing, async stats() { return { ...(await current)?.proxy.stats }; } };
}

async function serve({ root, outputDir, origins, headed = false }) {
  const external = loadBackend(root);
  const files = prepareOutput(outputDir);
  const policy = policyDetails(origins);
  const owner = ownedBrowser(external.chromium, origins, headed);
  const downloads = policy.downloads.size ? new ControlledDownloads({
    files, authorize: origin => origins.has(origin) && policy.downloads.has(origin), getContext: owner.existing, closeContext: context => owner.closeCaptured(context),
  }) : undefined;
  const closeOwned = permanent => {
    // Fence browser launches synchronously, before awaiting transfer teardown.
    const closing = owner.close(permanent);
    return downloads ? downloads.drain(() => closing, permanent) : closing;
  };
  const server = await external.createConnection({ browser: { isolated: false },
    webmcp: false, extension: false, capabilities: [], ignoreRoots: true,
    allowUnrestrictedFileAccess: false, saveSession: false,
    imageResponses: 'allow', outputDir: files, outputMaxSize: 16777216,
    timeouts: { action: 5000, navigation: 10000, idle: 0 },
  }, owner.get);
  server.registerCapabilities({ experimental: { [CAPABILITY]: { version: policy.version } } });
  let stopPromise;
  const stop = () => stopPromise ||= (async () => {
    // Mark closing before awaiting SDK disposal, so late calls cannot launch a replacement.
    const closing = closeOwned(true);
    await server.close().catch(() => {});
    await closing;
  })();
  const oldClose = server.onclose;
  server.onclose = () => {
    oldClose?.();
    const deadline = setTimeout(() => process.exit(1), 5000);
    void closeOwned(true).then(() => clearTimeout(deadline), () => process.exit(1));
  };
  await server.connect(new PolicyTransport(new external.StdioServerTransport(), origins, () => closeOwned(false), downloads));
  return { stop };
}

function options(argv) {
  const result = { headed: false, doctor: false };
  for (let i = 0; i < argv.length; i++) {
    const flag = argv[i];
    if (flag === '--headed') result.headed = true;
    else if (flag === '--doctor') result.doctor = true;
    else if (flag === '--backend-root' || flag === '--output-dir') {
      if (!argv[i + 1] || argv[i + 1].startsWith('--')) throw new Error('Missing backend option value');
      result[flag === '--backend-root' ? 'root' : 'outputDir'] = argv[++i];
    } else throw new Error('Unsupported browser backend option');
  }
  if (!result.root) throw new Error('--backend-root is required');
  return result;
}

async function main() {
  const config = options(process.argv.slice(2));
  const origins = parsePolicy(process.env[POLICY_ENV]);
  const external = loadBackend(config.root);
  if (config.doctor) {
    console.log(JSON.stringify({ version: policyDetails(origins).version, kind: 'doctor', browser_launched: false,
      mcp: MCP_VERSION, playwright: CORE_VERSION, capability: CAPABILITY,
      allowed_origin_count: origins.size, source_hashes: external.sourceHashes, downloads: policyDetails(origins).downloads.size ? 'controlled-http-unauthenticated' : 'deny-artifacts', uploads: 'deny', full_cdp_access: 'deny', download_authentication: policyDetails(origins).downloads.size ? 'explicit-owned-context-readonly-cookies' : 'deny', response_cookie_sync: policyDetails(origins).downloads.size ? 'explicit-native-interception' : 'deny', first_party_partitioned_cookie_reads: policyDetails(origins).downloads.size ? 'native-storage-on-request' : 'deny' }));
    return;
  }
  if (!config.outputDir) throw new Error('--output-dir is required');
  const { stop } = await serve({ ...config, origins });
  let stopping = false;
  const requestStop = () => {
    if (stopping) return;
    stopping = true;
    const deadline = setTimeout(() => process.exit(1), 5000);
    void stop().then(() => { clearTimeout(deadline); process.exitCode = 0; }, () => process.exit(1));
  };
  // The pinned SDK transport listens to data/error, not EOF. The process host
  // must feed EOF into the same owned cleanup path as explicit termination.
  process.stdin.once('end', requestStop);
  process.stdin.once('close', requestStop);
  process.stdout.on('error', requestStop);
  for (const signal of ['SIGTERM', 'SIGINT']) process.once(signal, requestStop);
  if (process.stdin.readableEnded || process.stdin.destroyed) requestStop();
}

if (require.main === module) main().catch(() => {
  // Do not print external exceptions, policy URLs, output paths or queries.
  process.stderr.write('Browser origin backend startup failed; check explicit policy, paths and pinned dependencies.\n');
  process.exitCode = 1;
});
module.exports = { loadBackend, ownedBrowser, serve, options, prepareOutput };

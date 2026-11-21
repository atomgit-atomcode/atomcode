import { existsSync, readFileSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { homedir } from 'node:os';

function baseHome() {
  return process.env.HUAWEICLOUD_HOME || homedir();
}

export function proxyConfigPath() {
  return join(baseHome(), '.config', 'huaweicloud', 'proxy.json');
}

export function readProxyConfig() {
  const path = proxyConfigPath();
  if (!existsSync(path)) return null;
  try {
    return JSON.parse(readFileSync(path, 'utf8'));
  } catch {
    return null;
  }
}

export function writeProxyConfig(config = {}) {
  const path = proxyConfigPath();
  mkdirSync(dirname(path), { recursive: true });
  const payload = {
    https_proxy: String(config.https_proxy || ''),
    http_proxy: String(config.http_proxy || ''),
    no_proxy: String(config.no_proxy || ''),
  };
  writeFileSync(path, JSON.stringify(payload, null, 2), { encoding: 'utf8' });
  return path;
}

export function clearProxyConfig() {
  const path = proxyConfigPath();
  if (!existsSync(path)) return false;
  rmSync(path);
  return true;
}

function shouldBypassProxy(hostname, noProxyList) {
  if (!noProxyList.length) return false;
  const lower = hostname.toLowerCase();
  for (const pattern of noProxyList) {
    const p = pattern.trim().toLowerCase();
    if (!p) continue;
    if (p === lower) return true;
    if (p === '*') return true;
    if (p.startsWith('*.')) {
      const domain = p.slice(1);
      if (lower.endsWith(domain) || lower === p.slice(2)) return true;
    }
    if (p.startsWith('.') && lower.endsWith(p)) return true;
    if (lower.endsWith('.' + p)) return true;
  }
  return false;
}

export function getProxySettings(targetUrl) {
  const envHttps = process.env.HTTPS_PROXY || process.env.https_proxy || '';
  const envHttp = process.env.HTTP_PROXY || process.env.http_proxy || '';
  const envNoProxy = process.env.NO_PROXY || process.env.no_proxy || '';

  const fileConfig = readProxyConfig();

  const https_proxy = envHttps || fileConfig?.https_proxy || fileConfig?.HTTPS_PROXY || '';
  const http_proxy = envHttp || fileConfig?.http_proxy || fileConfig?.HTTP_PROXY || '';
  const no_proxy = envNoProxy || fileConfig?.no_proxy || fileConfig?.NO_PROXY || '';

  if (!https_proxy && !http_proxy) return null;

  const noProxyList = no_proxy
    .split(',')
    .map((s) => s.trim())
    .filter(Boolean);

  if (targetUrl) {
    const parsed = new URL(targetUrl);
    if (shouldBypassProxy(parsed.hostname, noProxyList)) return null;
    const isHttps = parsed.protocol === 'https:';
    const proxyUrl = isHttps ? https_proxy : http_proxy;
    if (!proxyUrl) return null;
    return { proxyUrl, noProxyList, targetProtocol: parsed.protocol };
  }

  return { https_proxy, http_proxy, no_proxy, noProxyList };
}

export function getProxyUrlForTarget(targetUrl) {
  const settings = getProxySettings(targetUrl);
  if (!settings) return null;
  return settings.proxyUrl || null;
}

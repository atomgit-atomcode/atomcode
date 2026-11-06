import crypto from 'node:crypto';

import { resolveCredentialsWithRuntime } from '../auth/credentials.mjs';
import { getProxyDispatcher } from '../proxy/proxy-agent.mjs';

const BASE_URL = process.env.HWLINK_ENDPOINT || 'https://devstation.myhuaweicloud.com';

function sha256Hex(data) {
  return crypto.createHash('sha256').update(data).digest('hex');
}

function hmacSha256(key, data) {
  return crypto.createHmac('sha256', key).update(data).digest('hex');
}

function urlEncode(str) {
  const hex = (c) => '%' + (c < 16 ? '0' : '') + c.toString(16).toUpperCase();
  const noEscape = new Set('ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~'.split(''));
  let out = '';
  for (const ch of str) {
    const c = ch.codePointAt(0);
    out += noEscape.has(ch) && c < 0x80 ? ch : c < 0x80 ? hex(c) : encodeURIComponent(ch);
  }
  return out;
}

function timestamp() {
  return new Date().toISOString().replace(/[-:]/g, '').replace(/\.\d+/, '') + 'Z';
}

function sortedQs(query) {
  return Object.entries(query)
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(v)}`)
    .join('&');
}

function signRequest(method, path, query, body, ak, sk, securitytoken) {
  const ts = timestamp();
  const host = new URL(BASE_URL).host;

  const cqs = Object.entries(query)
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([k, v]) => `${urlEncode(k)}=${urlEncode(v)}`)
    .join('&');

  const curi =
    '/' +
    path
      .split('/')
      .filter(Boolean)
      .map((s) => urlEncode(s))
      .join('/') +
    '/';

  const signedHeaders = securitytoken ? 'host;x-sdk-date;x-security-token' : 'host;x-sdk-date';
  const canonicalHeaders = securitytoken
    ? `host:${host}\nx-sdk-date:${ts}\nx-security-token:${securitytoken}\n`
    : `host:${host}\nx-sdk-date:${ts}\n`;

  const bodyStr = body ? JSON.stringify(body) : '';
  const payloadHash = sha256Hex(bodyStr);

  const canonicalRequest = [method, curi, cqs, canonicalHeaders, signedHeaders, payloadHash].join('\n');

  const stringToSign = `SDK-HMAC-SHA256\n${ts}\n${sha256Hex(canonicalRequest)}`;
  const signature = hmacSha256(sk, stringToSign);

  const headers = {
    host,
    'x-sdk-date': ts,
    Authorization: `SDK-HMAC-SHA256 Access=${ak}, SignedHeaders=${signedHeaders}, Signature=${signature}`,
  };
  if (securitytoken) {
    headers['x-security-token'] = securitytoken;
  }
  return headers;
}

export function getCredentials() {
  const credentials = resolveCredentialsWithRuntime();
  return { ak: credentials.ak, sk: credentials.sk, securitytoken: credentials.securityToken };
}

async function apiGet(path, query, ak, sk, securitytoken) {
  query = query || {};
  const qs = sortedQs(query);
  const fullPath = qs ? `${path}?${qs}` : path;
  const headers = signRequest('GET', path, query, undefined, ak, sk, securitytoken);
  const url = `${BASE_URL}${fullPath}`;
  const dispatcher = await getProxyDispatcher(url);
  const fetchOpts = { headers };
  if (dispatcher) {
    fetchOpts.dispatcher = dispatcher;
    const { fetch: undiciFetch } = await import('undici');
    const resp = await undiciFetch(url, fetchOpts);
    return { status: resp.status, data: await resp.json() };
  }
  const resp = await fetch(url, fetchOpts);
  return { status: resp.status, data: await resp.json() };
}

async function apiPost(path, body, ak, sk, securitytoken) {
  const headers = signRequest('POST', path, {}, body, ak, sk, securitytoken);
  const url = `${BASE_URL}${path}`;
  const dispatcher = await getProxyDispatcher(url);
  const fetchOpts = {
    method: 'POST',
    headers: { ...headers, 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  };
  if (dispatcher) {
    fetchOpts.dispatcher = dispatcher;
    const { fetch: undiciFetch } = await import('undici');
    const resp = await undiciFetch(url, fetchOpts);
    return { status: resp.status, data: await resp.json() };
  }
  const resp = await fetch(url, fetchOpts);
  return { status: resp.status, data: await resp.json() };
}

export async function createConnection(envId, ak, sk, securitytoken) {
  const { status, data } = await apiPost(
    `/open-api-public/v1/devenvs/${envId}/connections`,
    { source: 'CLI' },
    ak,
    sk,
    securitytoken,
  );

  if (status !== 200 || data?.error_code !== '0000' || !data?.result?.connection_id) {
    throw new Error(`Failed to create connection: ${JSON.stringify(data)}`);
  }

  const connectionId = data.result.connection_id;
  const maxAttempts = 60;

  for (let i = 0; i < maxAttempts; i++) {
    process.stderr.write(`\rWaiting for connection ${connectionId}... (${i}s)`);
    const { data: getData } = await apiGet(
      `/open-api-public/v1/devenvs/${envId}/connections/${connectionId}`,
      {},
      ak,
      sk,
      securitytoken,
    );

    if (getData?.result?.connection_info?.url && getData.result.connection_info.extensions?.source != null) {
      const u = new URL(getData.result.connection_info.url);
      u.searchParams.set('source', String(getData.result.connection_info.extensions.source));
      process.stderr.write(`\rConnection ${connectionId} established (${i}s).\n`);
      return {
        wsUrl: u.toString(),
        source: getData.result.connection_info.extensions.source,
      };
    }
    await new Promise((resolve) => setTimeout(resolve, 1000));
  }

  throw new Error(`Timed out waiting for connection ${connectionId}`);
}

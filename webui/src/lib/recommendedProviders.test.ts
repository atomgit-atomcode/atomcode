import { test } from 'node:test';
import assert from 'node:assert';
import { landingRuntimesNotice, missingRecommended, recommendedNames } from './recommendedProviders.ts';
import type { MarketHelper } from '../api.ts';

function helper(name: string, id?: string): MarketHelper {
  return {
    name,
    package: id ?? `com.atomcode.${name}`,
    title: name,
    summary: '',
    version: '1.0',
    size_bytes: 1,
    sha256: '',
  };
}

const MARKET = [
  helper('tools'),
  helper('bash'),
  helper('unzip'),
  helper('tar'),
  helper('pkgconf'),
  helper('ninja'),
  helper('zig'),
];

test('a device with nothing installed is missing every recommended provider', () => {
  const missing = missingRecommended(MARKET, []);
  assert.deepEqual(
    missing.map((entry) => entry.name),
    ['bash', 'unzip', 'tar', 'pkgconf', 'tools', 'ninja'],
  );
});

test('installed providers drop out, and the rest keep the documented order', () => {
  const missing = missingRecommended(MARKET, ['com.atomcode.bash', 'com.atomcode.tools']);
  assert.deepEqual(
    missing.map((entry) => entry.name),
    ['unzip', 'tar', 'pkgconf', 'ninja'],
  );
});

test('what the market does not publish is not reported as a gap', () => {
  // A market that has not caught up yet must not make the panel claim a hole the
  // person cannot fill by clicking anything.
  const partial = [helper('bash')];
  const missing = missingRecommended(partial, []);
  assert.deepEqual(
    missing.map((entry) => entry.name),
    ['bash'],
  );
});

test('an unknowable install state reports nothing rather than guessing', () => {
  // `null` is "this page cannot see the device" (a desktop browser). Reporting
  // every provider as missing would send someone to install what they already have.
  assert.deepEqual(missingRecommended(MARKET, null), []);
});

test('the hint line names the packages a person has to install', () => {
  const missing = missingRecommended(MARKET, []);
  assert.equal(recommendedNames(missing), 'bash、unzip、tar、pkgconf、tools、ninja');
});

test('the landing notice stays silent unless something is actually missing', () => {
  // Nothing missing: no card on the page where a person writes their first line.
  assert.equal(landingRuntimesNotice([]), null);

  const missing = missingRecommended(MARKET, []);
  const notice = landingRuntimesNotice(missing);
  assert.equal(notice?.count, 6);
  assert.equal(notice?.names, 'bash、unzip、tar、pkgconf、tools、ninja');
});

test('a person who installed everything gets no landing card either', () => {
  const installed = MARKET.map((provider) => provider.package ?? provider.name);
  assert.equal(landingRuntimesNotice(missingRecommended(MARKET, installed)), null);
});

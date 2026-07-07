import assert from 'node:assert/strict';
import {
  normalizeDaemonEnvForUtf8Locale,
  normalizeDaemonEnvForUtf8LocalePlatform,
} from '../../src/daemon/env';

if (process.platform !== 'win32') {
  const env = normalizeDaemonEnvForUtf8Locale({
    LC_ALL: 'C',
    LANG: 'C',
  });

  assert.ok(
    Object.values(env).some((value) => value.toLowerCase().includes('utf')),
    `expected a UTF-8 locale in ${JSON.stringify(env)}`,
  );
}

{
  const env = normalizeDaemonEnvForUtf8Locale({
    LC_ALL: 'zh_CN.UTF-8',
    LANG: 'zh_CN.UTF-8',
  });

  assert.equal(env.LC_ALL, 'zh_CN.UTF-8');
  assert.equal(env.LANG, 'zh_CN.UTF-8');
}

if (process.platform !== 'win32') {
  const env = normalizeDaemonEnvForUtf8Locale({
    LC_CTYPE: 'C',
    LANG: 'UTF-8',
  });

  assert.ok(
    env.LC_CTYPE?.toLowerCase().includes('utf'),
    `expected LC_CTYPE to be UTF-8-capable in ${JSON.stringify(env)}`,
  );
}

{
  const env = normalizeDaemonEnvForUtf8LocalePlatform(
    {
      LC_CTYPE: 'UTF-8',
      LANG: 'C',
    },
    'linux',
  );

  assert.equal(env.LC_CTYPE, 'C.UTF-8');
}

{
  const env = normalizeDaemonEnvForUtf8LocalePlatform(
    {
      LC_CTYPE: 'UTF-8',
      LANG: 'C',
    },
    'darwin',
  );

  assert.equal(env.LC_CTYPE, 'UTF-8');
}

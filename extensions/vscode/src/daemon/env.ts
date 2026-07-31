function isCLocale(value: string | undefined): boolean {
  const normalized = (value ?? '').trim().toLowerCase();
  return normalized === '' || normalized === 'c' || normalized === 'posix';
}

function isUtf8Locale(value: string | undefined): boolean {
  const normalized = (value ?? '').trim().toLowerCase();
  return normalized.includes('utf-8') || normalized.includes('utf8');
}

function isBareUtf8Locale(value: string | undefined): boolean {
  const normalized = (value ?? '').trim().toLowerCase();
  return normalized === 'utf-8' || normalized === 'utf8';
}

function isDarwin(platform: string): boolean {
  return platform === 'darwin';
}

function utf8CtypeFallback(platform: string): string {
  return isDarwin(platform) ? 'UTF-8' : 'C.UTF-8';
}

function utf8LangFallback(platform: string): string {
  return isDarwin(platform) ? 'en_US.UTF-8' : 'C.UTF-8';
}

export function normalizeDaemonEnvForUtf8LocalePlatform(
  env: NodeJS.ProcessEnv,
  platform: string,
): NodeJS.ProcessEnv {
  if (platform === 'win32') {
    return { ...env };
  }

  const next: NodeJS.ProcessEnv = { ...env };
  if (isUtf8Locale(next.LC_ALL) && !isBareUtf8Locale(next.LC_ALL)) {
    return next;
  }
  if (isCLocale(next.LC_ALL) || isBareUtf8Locale(next.LC_ALL)) {
    delete next.LC_ALL;
  }

  const hasUtf8Ctype =
    isUtf8Locale(next.LC_CTYPE) && (isDarwin(platform) || !isBareUtf8Locale(next.LC_CTYPE));
  if (!hasUtf8Ctype) {
    next.LC_CTYPE = utf8CtypeFallback(platform);
  }
  if (isCLocale(next.LANG) || isBareUtf8Locale(next.LANG)) {
    next.LANG = utf8LangFallback(platform);
  }
  return next;
}

export function normalizeDaemonEnvForUtf8Locale(env: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
  return normalizeDaemonEnvForUtf8LocalePlatform(env, process.platform);
}

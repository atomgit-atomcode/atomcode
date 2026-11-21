import { spawn } from 'node:child_process';

import { classifyHcloudArgs, redactSecrets, assertAllowed } from './safety-policy.mjs';
import { getProxySettings } from './proxy/proxy-config.mjs';

const DEFAULT_TIMEOUT_MS = 60_000;
const DEFAULT_FORCE_KILL_AFTER_MS = 2_000;
const DEFAULT_MAX_RETRIES = 1;

export function planHcloudCommand(args, options = {}) {
  const normalizedArgs = Array.isArray(args) ? args.map(String) : [];
  const classification = classifyHcloudArgs(normalizedArgs, options);
  const command = ['hcloud', ...normalizedArgs].map((arg) => quoteShellArg(arg)).join(' ');
  const warnings = planningWarnings(normalizedArgs);
  const paramValidation = validateRequiredParams(normalizedArgs);
  if (paramValidation.missing.length > 0) {
    warnings.push('Missing required parameters: ' + paramValidation.missing.join(', '));
    if (paramValidation.hints && paramValidation.hints.length > 0) {
      warnings.push('Find valid values: ' + paramValidation.hints.join('; '));
    }
  }
  return {
    executable: 'hcloud',
    args: redactSecrets(normalizedArgs),
    command: redactOutput(command),
    executableBlock: redactOutput(command),
    warnings,
    classification,
    safeToRun: classification.decision === 'allow',
  };
}

export async function runHcloud(args, options = {}) {
  const normalizedArgs = Array.isArray(args) ? args.map(String) : [];
  const plan = {
    ...planHcloudCommand(normalizedArgs, options),
    rawArgs: normalizedArgs,
  };
  assertAllowed(plan.classification);

  const maxRetries = options.maxRetries ?? DEFAULT_MAX_RETRIES;
  for (let attempt = 0; attempt <= maxRetries; attempt += 1) {
    const result = await runHcloudOnce(plan, options);
    if (result.ok || attempt >= maxRetries || !isRetryableNetworkError(result)) {
      return {
        ...result,
        retries: attempt,
        attempts: attempt + 1,
      };
    }
    await wait((options.retryBaseDelayMs ?? 500) * 2 ** attempt);
  }
  throw new Error('Unreachable retry state.');
}

function runHcloudOnce(plan, options) {
  const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  const forceKillAfterMs = options.forceKillAfterMs ?? DEFAULT_FORCE_KILL_AFTER_MS;
  const executable = options.executable || options.env?.HCLOUD_BIN || process.env.HCLOUD_BIN || 'hcloud';
  const executableArgs = Array.isArray(options.executableArgs) ? options.executableArgs.map(String) : [];
  const cwd = options.cwd || undefined;

  return new Promise((resolve) => {
    const proxySettings = getProxySettings();
    const proxyEnv = {};
    if (proxySettings) {
      if (proxySettings.https_proxy) proxyEnv.HTTPS_PROXY = proxySettings.https_proxy;
      if (proxySettings.http_proxy) proxyEnv.HTTP_PROXY = proxySettings.http_proxy;
      if (proxySettings.no_proxy) proxyEnv.NO_PROXY = proxySettings.no_proxy;
    }
    const child = spawn(executable, [...executableArgs, ...plan.rawArgs], {
      shell: false,
      windowsHide: true,
      cwd,
      env: {
        ...process.env,
        ...proxyEnv,
        ...options.env,
      },
    });
    if (options.stdin) {
      if (typeof options.stdin === 'function') {
        options.stdin(child.stdin);
      } else {
        child.stdin.write(String(options.stdin));
        child.stdin.end();
      }
    }
    let stdout = '';
    let stderr = '';
    let timedOut = false;
    let settled = false;
    let forceTimer;
    let settleTimer;

    function finish(result) {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      clearTimeout(forceTimer);
      clearTimeout(settleTimer);
      resolve(result);
    }

    const timer = setTimeout(() => {
      timedOut = true;
      child.kill('SIGTERM');
      forceTimer = setTimeout(() => child.kill('SIGKILL'), forceKillAfterMs);
      settleTimer = setTimeout(() => {
        finish({
          ok: false,
          code: 'TIMEOUT',
          error: `hcloud command timed out after ${timeoutMs} ms.`,
          stdout: redactOutput(stdout),
          stderr: redactOutput(stderr),
          plan,
        });
      }, forceKillAfterMs + 500);
    }, timeoutMs);

    child.stdout.on('data', (chunk) => {
      stdout += chunk.toString('utf8');
    });
    child.stderr.on('data', (chunk) => {
      stderr += chunk.toString('utf8');
    });
    child.on('error', (error) => {
      finish({
        ok: false,
        code: 'SPAWN_ERROR',
        error: error.message,
        plan,
      });
    });
    child.on('close', (code, signal) => {
      if (timedOut) {
        finish({
          ok: false,
          code: 'TIMEOUT',
          error: `hcloud command timed out after ${timeoutMs} ms.`,
          exitCode: code,
          signal,
          stdout: redactOutput(stdout),
          stderr: redactOutput(stderr),
          plan,
        });
        return;
      }
      const apiError = extractApiError(stdout);
      if (apiError) {
        finish({
          ok: false,
          exitCode: code,
          signal,
          errorCode: apiError.errorCode,
          errorMessage: apiError.errorMessage,
          stdout: redactOutput(stdout),
          stderr: redactOutput(stderr),
          plan,
        });
        return;
      }
      finish({
        ok:
          code === 0 ||
          (code !== 0 &&
            /successfully|succ?ess.*\[200\]|create bucket successfully|upload successfully/i.test(stdout + stderr)),
        exitCode: code,
        signal,
        stdout: redactOutput(stdout),
        stderr: redactOutput(stderr),
        plan,
      });
    });
  });
}

function isRetryableNetworkError(result) {
  if (result.code === 'TIMEOUT') return false;
  const text = `${result.error || ''}\n${result.stdout || ''}\n${result.stderr || ''}`;
  return /\[NETWORK_ERROR\]|connection timed out|ECONNRESET|ETIMEDOUT|temporary failure|TLS handshake timeout/i.test(
    text,
  );
}

function wait(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function quoteShellArg(value) {
  const text = String(value);
  if (!text) return '""';
  if (/^[A-Za-z0-9_./:=@-]+$/.test(text)) return text;
  return `"${text.replace(/(["\\])/g, '\\$1')}"`;
}

function planningWarnings(args) {
  const joined = args.join(' ');
  const warnings = [];
  if (/adminPass|password|passwd|secret|token/i.test(joined)) {
    warnings.push(
      'This command appears to contain a password or secret field. Do not leave plaintext secrets in shell history; prefer local-only input or a runtime injection pattern.',
    );
  }
  return warnings;
}

const REQUIRED_PARAMS = {
  'ECS CreateServers': ['server.flavorRef', 'server.imageRef', 'server.nics.1.subnet_id'],
  'VPC CreateVpc': ['vpc.cidr'],
  'VPC CreateSubnet': ['subnet.vpc_id', 'subnet.cidr'],
  'VPC CreateSecurityGroupRule': [
    'security_group_rule.security_group_id',
    'security_group_rule.direction',
    'security_group_rule.protocol',
  ],
  'EIP CreatePublicip': ['bandwidth.share_type', 'publicip.type'],
  'FunctionGraph CreateFunction': ['func_name', 'runtime', 'handler', 'memory_size', 'package', 'timeout'],
  'FunctionGraph CreateFunctionTrigger': ['function_urn', 'trigger_type_code'],
  'APIG CreateInstanceV2': ['spec_id'],
  'OBS mb': ['obs://'],
  'OBS rm': ['obs://'],
};

const PARAM_VALUE_HINTS = {
  'server.flavorRef': 'Run `hcloud ECS ListFlavors --cli-region=<r>` to find valid flavors',
  'server.imageRef': 'Run `hcloud IMS ListImages --cli-region=<r> --__imagetype=gold` to find valid image IDs',
  'server.nics.1.subnet_id': 'Run `hcloud VPC ListSubnets --cli-region=<r>` to find subnet IDs',
  'subnet.vpc_id': 'Run `hcloud VPC ListVpcs --cli-region=<r>` to find VPC IDs',
  func_name: 'Function name must be unique within project',
  runtime: 'Run `hcloud FunctionGraph ListRuntimes` to see available runtimes',
  spec_id: 'APIG spec: BASIC (no public IP) or PROFESSIONAL (requires --loadbalancer_provider)',
  'obs://': 'Bucket name must be globally unique and DNS-compliant (lowercase, numbers, hyphens only)',
};

function validateRequiredParams(args) {
  if (!args || args.length < 2) return { valid: true, missing: [], hints: [] };
  const key = `${args[0]} ${args[1]}`;
  const required = REQUIRED_PARAMS[key];
  if (!required) return { valid: true, missing: [], hints: [] };
  const argsStr = args.join(' ');
  const missing = required.filter((param) => !argsStr.includes(param));
  const hints = missing.map((param) => PARAM_VALUE_HINTS[param]).filter(Boolean);
  return { valid: missing.length === 0, missing, hints };
}

function extractApiError(stdout) {
  let text = String(stdout || '');
  // Strip KooCLI multi-version prefix lines (e.g. "ListVpcs有多个版本,默认使用该API版本v3…")
  const bracketIdx = text.indexOf('{');
  if (bracketIdx > 0) text = text.substring(bracketIdx);
  try {
    const parsed = JSON.parse(text);
    if (parsed.error_code || parsed.errorCode) {
      return {
        errorCode: parsed.error_code || parsed.errorCode || 'UNKNOWN',
        errorMessage: parsed.error_msg || parsed.errorMsg || parsed.message || '',
      };
    }
    if (parsed.error && typeof parsed.error === 'object') {
      return {
        errorCode: parsed.error.code || parsed.error.error_code || 'UNKNOWN',
        errorMessage: parsed.error.message || parsed.error.error_msg || '',
      };
    }
  } catch {}
  const ecMatch = text.match(/"error_code"\s*:\s*"([^"]+)"/);
  const emMatch = text.match(/"error_msg"\s*:\s*"([^"]+)"/);
  if (ecMatch) {
    return { errorCode: ecMatch[1], errorMessage: emMatch ? emMatch[1] : '' };
  }
  return null;
}

export function redactOutput(output) {
  let text = String(output || '');
  const bracketIdx = text.indexOf('{');
  if (bracketIdx > 0) text = text.substring(bracketIdx);
  try {
    return JSON.stringify(redactSecrets(JSON.parse(text)), null, 2);
  } catch {
    return redactSecrets(text);
  }
}

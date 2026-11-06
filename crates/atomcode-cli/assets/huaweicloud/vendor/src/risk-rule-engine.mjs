import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const defaultRulesPath = join(__dirname, '..', 'safety', 'rules', 'cloud-risk-rules.json');

const SEVERITY_RANK = {
  deny: 3,
  warn: 2,
  info: 1,
};

export function loadRiskRules(options = {}) {
  const path = options.path || defaultRulesPath;
  return JSON.parse(readFileSync(path, 'utf8'));
}

function redactEvidence(text) {
  return String(text)
    .replace(
      /((?:access[_-]?key|secret[_-]?key|security[_-]?token|x[_-]?auth[_-]?token|authorization|password|passwd|adminPass|credential)\s*[:=]\s*)("[^"]*"|'[^']*'|[^\s,;]+)/gi,
      '$1<redacted>',
    )
    .replace(/(AK|SK)\s*[:=]\s*("[^"]*"|'[^']*'|[^\s,;]+)/g, '$1=<redacted>');
}

function normalizeText(value) {
  if (typeof value === 'string') return value;
  return JSON.stringify(value, null, 2);
}

function evaluationContext(stage, input) {
  if (stage === 'command') {
    const command = normalizeText(input.command || input.text || '');
    return { text: command, command };
  }
  if (stage === 'artifact') {
    const path = String(input.path || '');
    const content = normalizeText(input.content || '');
    return { text: `${path}\n${content}`, path, content };
  }
  if (stage === 'deploy_plan') {
    const plan = normalizeText(input.plan || input.text || input);
    return { text: plan, plan };
  }
  return { text: normalizeText(input) };
}

function conditionMatches(condition, context) {
  const field = condition.field || 'text';
  const value = Object.hasOwn(context, field) ? context[field] : context.text;
  return new RegExp(condition.regex, 'ims').test(String(value || ''));
}

function ruleMatches(rule, context) {
  const match = rule.match || {};
  const all = match.all;
  const any = match.any;
  const none = match.none;
  if (Array.isArray(all) && !all.every((condition) => conditionMatches(condition, context))) {
    return false;
  }
  if (Array.isArray(any) && !any.some((condition) => conditionMatches(condition, context))) {
    return false;
  }
  if (Array.isArray(none) && none.some((condition) => conditionMatches(condition, context))) {
    return false;
  }
  return Array.isArray(all) || Array.isArray(any);
}

function excerpt(text) {
  const compact = redactEvidence(String(text).replace(/\s+/g, ' ').trim());
  if (compact.length <= 240) return compact;
  return `${compact.slice(0, 237)}...`;
}

function evaluate(stage, inputs, options = {}) {
  const catalog = options.catalog || loadRiskRules(options);
  const items = Array.isArray(inputs) ? inputs : [inputs];
  const findings = [];

  for (const input of items) {
    const context = evaluationContext(stage, input || {});
    for (const rule of catalog.rules) {
      if (!rule.stages.includes(stage)) continue;
      if (!ruleMatches(rule, context)) continue;
      findings.push({
        ruleId: rule.id,
        title: rule.title,
        category: rule.category,
        severity: rule.severity,
        message: rule.message,
        remediation: rule.remediation,
        source: input?.path || stage,
        evidence: excerpt(context.text),
      });
    }
  }

  findings.sort((a, b) => SEVERITY_RANK[b.severity] - SEVERITY_RANK[a.severity]);
  const hasDeny = findings.some((finding) => finding.severity === 'deny');
  const hasWarn = findings.some((finding) => finding.severity === 'warn');
  return {
    decision: hasDeny ? 'deny' : hasWarn ? 'warn' : 'allow',
    findings,
  };
}

export function evaluateCommandRisk(command, options = {}) {
  return evaluate('command', { command }, options);
}

export function evaluateArtifacts(artifacts, options = {}) {
  return evaluate('artifact', Array.isArray(artifacts) ? artifacts : [], options);
}

export function evaluateDeployPlan(plan, options = {}) {
  return evaluate('deploy_plan', { plan }, options);
}

export function mergeRiskDecision(base, risk) {
  if (!risk || !risk.findings?.length) return base;
  if (risk.decision === 'deny') {
    const topFinding = risk.findings[0];
    return {
      ...base,
      decision: 'deny',
      risk: topFinding.category,
      reason: topFinding.message,
      blockedByRiskRule: true,
      findings: risk.findings,
    };
  }
  return {
    ...base,
    warnings: [...(base.warnings || []), ...risk.findings],
  };
}

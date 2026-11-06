import { readFileSync, readdirSync, existsSync, writeFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { homedir } from 'node:os';
import { spawnSync } from 'node:child_process';

import { evaluateArtifacts, evaluateCommandRisk, evaluateDeployPlan } from './risk-rule-engine.mjs';
import { classifyTextCommand, redactSecrets } from './safety-policy.mjs';
import { planHcloudCommand, runHcloud } from './hcloud-cli.mjs';
import { searchMarketplace } from './search-market.mjs';
import { getServiceIcon } from './icon-library.mjs';
import {
  execWithSession,
  execOneShot,
  closeSession,
  uploadFileWithSession,
  uploadProjectWithSession,
  getCurrentWorkspaceId,
  setWorkspaceId,
} from './sandbox/session-manager.mjs';
import { hdkitCheckUser, hdkitSignAgreement, hdkitConnect, hdkitCredentials } from './sandbox/hdkitservice-api.mjs';
import { getAuthStatus, syncAuth } from './auth/service.mjs';
import {
  readGlobalCredentials,
  writeObsConfig as writeObsConfigFile,
  setRuntimeCredentials,
  clearRuntimeCredentials,
} from './auth/credentials.mjs';

const __dirname = dirname(fileURLToPath(import.meta.url));
const SKILLS_ROOT_DEV = join(__dirname, '..', 'skills');
function opencodeSkillsDir() {
  const home = homedir();
  return join(home, '.config', 'opencode', 'skills');
}
function codeartsSkillsDir() {
  const home = homedir();
  return join(home, '.codeartsdoer', 'skills');
}
function workbuddySkillsDir() {
  const home = homedir();
  return join(home, '.workbuddy', 'skills');
}
function dshSkillsDir() {
  const home = process.env.DSH_HOME || join(homedir(), '.dsh');
  return join(home, 'skills');
}
function readOfficeaceRegistryInstallDir() {
  if (process.platform !== 'win32') return null;
  try {
    const r = spawnSync('reg', ['query', 'HKCU\\SOFTWARE\\OfficeAce\\OfficeAce', '/v', 'InstallDir'], {
      encoding: 'utf8',
      windowsHide: true,
      timeout: 5000,
    });
    if (r.status === 0) {
      const m = r.stdout.match(/InstallDir\s+REG_SZ\s+(.+)/);
      if (m) return m[1].trim();
    }
  } catch {}
  return null;
}

function officeaceSkillsRoot() {
  const configRoot = process.env.OFFICE_CLAW_CONFIG_ROOT;
  if (configRoot && existsSync(join(configRoot, 'capabilities.json'))) return join(configRoot, 'skills');
  const regDir = readOfficeaceRegistryInstallDir();
  if (regDir) {
    const dir = join(regDir, '.office-claw', 'skills');
    if (existsSync(dir)) return dir;
  }
  if (process.platform === 'win32') {
    const bases = [process.env.ProgramFiles, 'C:\\Program Files', 'D:\\Program Files'];
    if (process.env.LOCALAPPDATA) bases.push(join(process.env.LOCALAPPDATA, 'Programs'));
    for (const base of bases) {
      if (!base) continue;
      const dir = join(base, 'OfficeAce', '.office-claw', 'skills');
      if (existsSync(dir)) return dir;
    }
  }
  return null;
}

function hermesSkillsDir() {
  if (process.env.HERMES_HOME) return join(process.env.HERMES_HOME, 'skills');
  // Hermes on Windows stores under LOCALAPPDATA, not ~/.hermes
  if (process.platform === 'win32' && process.env.LOCALAPPDATA) {
    return join(process.env.LOCALAPPDATA, 'hermes', 'skills');
  }
  const home = homedir();
  return join(home, '.hermes', 'skills');
}

export function listSkillDirs(root) {
  if (!existsSync(root)) return [];
  try {
    return readdirSync(root, { withFileTypes: true })
      .filter((d) => (d.isDirectory() || d.isSymbolicLink()) && existsSync(join(root, d.name, 'SKILL.md')))
      .map((d) => d.name);
  } catch {
    return [];
  }
}

export function findSkillsRoot(candidates) {
  for (const dir of candidates) {
    if (listSkillDirs(dir).length > 0) return dir;
  }
  return null;
}

function resolveSkillsRoot() {
  return (
    findSkillsRoot([
      SKILLS_ROOT_DEV,
      dshSkillsDir(),
      codeartsSkillsDir(),
      opencodeSkillsDir(),
      workbuddySkillsDir(),
      officeaceSkillsRoot(),
      hermesSkillsDir(),
    ]) || SKILLS_ROOT_DEV
  );
}
const SKILLS_ROOT = resolveSkillsRoot();

export const TOOL_DEFINITIONS = [
  {
    name: 'huaweicloud_check_cli',
    description: 'Check whether Huawei Cloud KooCLI hcloud is installed. Returns redacted output.',
    inputSchema: {
      type: 'object',
      properties: {},
    },
  },
  {
    name: 'huaweicloud_plan_cli_command',
    description: 'Classify and plan a Huawei Cloud hcloud command without executing it.',
    inputSchema: {
      type: 'object',
      required: ['args'],
      properties: {
        args: {
          type: 'array',
          items: { type: 'string' },
          description: 'hcloud arguments, excluding the hcloud executable.',
        },
        allowWrites: {
          type: 'boolean',
          description: 'Only true after explicit user approval for this exact operation.',
        },
      },
    },
  },
  {
    name: 'huaweicloud_run_readonly_command',
    description: 'Run a read-only hcloud command through the toolkit safety policy and redact output.',
    inputSchema: {
      type: 'object',
      required: ['args'],
      properties: {
        args: {
          type: 'array',
          items: { type: 'string' },
        },
        timeoutMs: {
          type: 'number',
          description: 'Optional timeout in milliseconds. Defaults to 60000.',
        },
        maxRetries: {
          type: 'number',
          description: 'Optional retry count for transient network errors. Defaults to 1.',
        },
        cwd: {
          type: 'string',
          description: 'Optional working directory for the hcloud process.',
        },
      },
    },
  },
  {
    name: 'huaweicloud_list_operations',
    description:
      'List KooCLI operations for a Huawei Cloud service by running local/read-only hcloud <Service> --help.',
    inputSchema: {
      type: 'object',
      required: ['service'],
      properties: {
        service: {
          type: 'string',
          description: 'KooCLI service name, such as ECS, VPC, IMS, OBS, RDS, or CDN.',
        },
        timeoutMs: {
          type: 'number',
          description: 'Optional timeout in milliseconds. Defaults to 60000.',
        },
      },
    },
  },
  {
    name: 'huaweicloud_run_approved_command',
    description:
      'Run a write-capable hcloud command only after the exact command has been shown and explicitly approved by the user.',
    inputSchema: {
      type: 'object',
      required: ['args', 'approvedCommand', 'approvedByUser'],
      properties: {
        args: {
          type: 'array',
          items: { type: 'string' },
          description: 'hcloud arguments, excluding the hcloud executable.',
        },
        approvedCommand: {
          type: 'string',
          description: 'The exact command string previously shown to the user.',
        },
        approvedByUser: {
          type: 'boolean',
          description: 'Must be true only after the user explicitly approves this exact command.',
        },
        timeoutMs: {
          type: 'number',
          description: 'Optional timeout in milliseconds. Defaults to 60000.',
        },
        maxRetries: {
          type: 'number',
          description: 'Optional retry count for transient network errors. Defaults to 1.',
        },
        cwd: {
          type: 'string',
          description: 'Optional working directory for the hcloud process.',
        },
      },
    },
  },
  {
    name: 'huaweicloud_show_profile_redacted',
    description: 'Inspect a KooCLI profile through hcloud configure show and return only redacted output.',
    inputSchema: {
      type: 'object',
      properties: {
        profile: {
          type: 'string',
          description: 'Optional KooCLI profile name.',
        },
      },
    },
  },
  {
    name: 'huaweicloud_hook_check_command',
    description: 'Check a planned shell or hcloud command against Huawei Cloud hook risk rules without executing it.',
    inputSchema: {
      type: 'object',
      required: ['command'],
      properties: {
        command: { type: 'string', description: 'The exact command text to inspect.' },
      },
    },
  },
  {
    name: 'huaweicloud_hook_check_artifacts',
    description: 'Check generated code, IaC, policy, or config artifacts against Huawei Cloud hook risk rules.',
    inputSchema: {
      type: 'object',
      required: ['artifacts'],
      properties: {
        artifacts: {
          type: 'array',
          items: {
            type: 'object',
            required: ['path', 'content'],
            properties: {
              path: { type: 'string' },
              content: { type: 'string' },
            },
          },
        },
      },
    },
  },
  {
    name: 'huaweicloud_hook_check_deploy_plan',
    description:
      'Check a structured or textual deployment plan for Huawei Cloud sandbox, exposure, IAM, and cost risks.',
    inputSchema: {
      type: 'object',
      required: ['plan'],
      properties: {
        plan: {
          description: 'Deployment plan as an object, array, or string.',
        },
      },
    },
  },
  {
    name: 'huaweicloud_service_catalog',
    description: 'Return the recommended capability sources for Huawei Cloud agent tasks.',
    inputSchema: {
      type: 'object',
      properties: {
        intent: {
          type: 'string',
          description: 'Developer intent to route, such as deploy app, use API, debug error, or inspect resources.',
        },
      },
    },
  },
  {
    name: 'huaweicloud_explain_error',
    description: 'Explain a Huawei Cloud CLI, API, SDK, or agent workflow error and suggest next diagnostic steps.',
    inputSchema: {
      type: 'object',
      properties: {
        service: { type: 'string' },
        errorCode: { type: 'string' },
        message: { type: 'string' },
        requestId: { type: 'string' },
      },
    },
  },
  {
    name: 'huaweicloud_search_docs',
    description:
      'Search across Huawei Cloud SKILL.md files and local documentation. Returns top 10 relevant results with source, name, snippet, and relevance score. Use when the agent needs to discover which skill covers a topic, or when uncertain about API parameters, quotas, or limitations.',
    inputSchema: {
      type: 'object',
      required: ['query'],
      properties: {
        query: { type: 'string', description: 'Search query across skill descriptions and documentation.' },
        topic: {
          type: 'string',
          description: 'Optional filter: all | ecs | obs | vpc | iam | rds | cce | modelarts | dew. Defaults to all.',
        },
      },
    },
  },
  {
    name: 'huaweicloud_retrieve_skill',
    description:
      'Retrieve a full SKILL.md by skill name. Returns the complete skill content plus list of reference files. Use when the agent has identified which skill to load and needs the full procedure.',
    inputSchema: {
      type: 'object',
      required: ['name'],
      properties: {
        name: { type: 'string', description: 'Skill name, e.g., huaweicloud-core, huawei-ecs, huawei-obs.' },
      },
    },
  },
  {
    name: 'huaweicloud_list_regions',
    description:
      'List available Huawei Cloud regions. Returns region IDs, display names, and endpoints. Use when the agent needs to discover available regions before creating resources.',
    inputSchema: {
      type: 'object',
      properties: {},
    },
  },
  {
    name: 'huaweicloud_get_regional_availability',
    description:
      'Check if a specific Huawei Cloud service is available in a target region. Use before creating resources to prevent failures from regional unavailability.',
    inputSchema: {
      type: 'object',
      required: ['service', 'region'],
      properties: {
        service: {
          type: 'string',
          description: 'Service name: ecs, obs, rds, gaussdb, cce, modelarts, functiongraph, etc.',
        },
        region: { type: 'string', description: 'Region ID: cn-south-1, cn-north-4, ap-southeast-3, etc.' },
      },
    },
  },
  {
    name: 'huaweicloud_search_marketplace',
    description:
      'Search the Huawei Cloud agent skill marketplace for available skills. Returns scored results with names, categories, and descriptions. Use when built-in skills are insufficient or the user asks what skills exist.',
    inputSchema: {
      type: 'object',
      properties: {
        query: { type: 'string', description: 'Search query across skill name, description, triggers, and service.' },
        category: {
          type: 'string',
          description: 'Optional category filter: computing, storage, network, security, devtools, monitoring, etc.',
        },
      },
    },
  },
  {
    name: 'huaweicloud_get_service_icon',
    description:
      'Find the official Huawei Cloud service logo from the Huawei Cloud Icons library (open.huaweicloud.com/openplatform/icons.html). Returns top 5 matches with CDN logo URLs, local paths, category, aliases, and product page links. Provide service (e.g. ecs, obs, modelarts, 对象存储) or category (e.g. 计算, 存储, 人工智能) to browse. Use when generating PPT, architecture diagrams (draw.io), or frontend pages that need official Huawei Cloud service logos.',
    inputSchema: {
      type: 'object',
      properties: {
        service: {
          type: 'string',
          description:
            'Service name, alias, or Chinese name, e.g. ecs, obs, modelarts, 对象存储, 虚拟私有云. Omit to browse by category only.',
        },
        category: {
          type: 'string',
          description: 'Optional category filter, e.g. 计算, 存储, 网络, 人工智能, 数据库, 安全, 企业应用.',
        },
      },
    },
  },
  {
    name: 'huaweicloud_setup_obs_config',
    description:
      'Synchronize KooCLI credentials to OBS config (~/.obsutilconfig). KooCLI and OBS use separate credential stores — hcloud commands work fine but OBS commands fail with "Please set ak, sk" unless this sync is done. Run this once to enable OBS operations; re-run after changing hcloud credentials.',
    inputSchema: {
      type: 'object',
      properties: {
        profile: { type: 'string', description: 'Optional KooCLI profile name. Uses the active profile by default.' },
      },
    },
  },
  {
    name: 'huaweicloud_auth_status',
    description:
      'Check unified Huawei Cloud authentication status across the global credential vault, OBS, KooCLI, and all supported agent MCP registrations. Returns only redacted/status information, never credentials.',
    inputSchema: {
      type: 'object',
      properties: {
        target: {
          type: 'string',
          description:
            'Agent target to check: opencode, codex, codex-desktop, codearts, workbuddy, dsh, officeace, hermes, openclaw, atomcode, or all (default).',
        },
      },
    },
  },
  {
    name: 'huaweicloud_auth_sync',
    description:
      'Synchronize credentials from the global Huawei Cloud credential vault to OBS and report agent registration status. Does not write secrets into any agent config.',
    inputSchema: {
      type: 'object',
      properties: {
        target: {
          type: 'string',
          description:
            'Agent target to report after sync: opencode, codex, codex-desktop, codearts, workbuddy, dsh, officeace, hermes, or all (default).',
        },
      },
    },
  },
  {
    name: 'huaweicloud_auth_init',
    description:
      'Set or clear runtime Huawei Cloud credentials (AK/SK) for this MCP session. Runtime credentials take highest priority over environment variables and config files for all subsequent API calls. Use when switching accounts within the same Agent session — call with AK/SK to switch, or with clear=true to fall back to env/file credentials.',
    inputSchema: {
      type: 'object',
      properties: {
        ak: { type: 'string', description: 'Huawei Cloud Access Key (required unless clear=true)' },
        sk: { type: 'string', description: 'Huawei Cloud Secret Key (required unless clear=true)' },
        region: { type: 'string', description: 'Default region (optional)' },
        clear: { type: 'boolean', description: 'Set to true to clear runtime credentials and revert to env/file' },
      },
    },
  },
  {
    name: 'huaweicloud_sandbox_exec_with_session',
    description:
      'Execute a command on a workspace terminal with session reuse (state persists across calls). Shell state (cd, env vars, aliases) carries over between calls. Use for interactive work and command sequences that need shared state. NOT for long-running commands (>30s) — prefer exec_one_shot for those.',
    inputSchema: {
      type: 'object',
      required: ['command'],
      properties: {
        command: { type: 'string', description: 'The shell command to execute on the remote workspace' },
        workspace_id: {
          type: 'string',
          description:
            'Workspace ID from huaweicloud_sandbox_connect return value. Required - must be passed explicitly when HW_WORKSPACE_ID is not set.',
        },
        username: { type: 'string', description: 'Login username for the remote terminal (default: root)' },
        timeout_ms: { type: 'number', description: 'Execution timeout in milliseconds (default: 120000)' },
      },
    },
  },
  {
    name: 'huaweicloud_sandbox_exec_one_shot',
    description:
      'Execute a command on a workspace terminal with a fresh connection per call (no session state carries over). Each invocation opens a new WebSocket connection, executes one command, then disconnects. Use for long-running build/deploy/install commands (>30s) that do not need shell state persistence between calls. More stable than session-based execution for heavy workloads.',
    inputSchema: {
      type: 'object',
      required: ['command'],
      properties: {
        command: { type: 'string', description: 'The shell command to execute on the remote workspace' },
        workspace_id: {
          type: 'string',
          description:
            'Workspace ID from huaweicloud_sandbox_connect return value. Required - must be passed explicitly when HW_WORKSPACE_ID is not set.',
        },
        username: { type: 'string', description: 'Login username for the remote terminal (default: root)' },
        timeout_ms: { type: 'number', description: 'Execution timeout in milliseconds (default: 120000)' },
      },
    },
  },
  {
    name: 'huaweicloud_sandbox_close_session',
    description: 'Close the persistent terminal session for a workspace.',
    inputSchema: {
      type: 'object',
      properties: {
        workspace_id: {
          type: 'string',
          description:
            'Workspace ID from huaweicloud_sandbox_connect return value. Required - must be passed explicitly when HW_WORKSPACE_ID is not set.',
        },
        username: { type: 'string', description: 'Login username (default: root)' },
      },
    },
  },
  {
    name: 'huaweicloud_sandbox_upload_file',
    description:
      'Upload a local file into the sandbox workspace. Base64-encodes the file, writes it in small chunks through the terminal session (the exec channel is fragile for large single commands), then decodes and verifies the md5 checksum. Use this instead of embedding large file content directly in a command.',
    inputSchema: {
      type: 'object',
      required: ['local_path', 'remote_path'],
      properties: {
        local_path: { type: 'string', description: 'Absolute path to the local file to upload.' },
        remote_path: { type: 'string', description: 'Target path in the sandbox, e.g. /workspace/<repo>/index.html.' },
        workspace_id: {
          type: 'string',
          description:
            'Workspace ID from huaweicloud_sandbox_connect return value. Required - must be passed explicitly when HW_WORKSPACE_ID is not set.',
        },
        username: { type: 'string', description: 'Login username (default: root)' },
        timeout_ms: { type: 'number', description: 'Per-command execution timeout in milliseconds (default: 120000)' },
      },
    },
  },
  {
    name: 'huaweicloud_sandbox_upload_project',
    description:
      'Package a local project directory and upload it to a sandbox workspace via HTTP tunnel. Falls back to base64 chunking if tunnel fails. Creates a tar.gz archive, uploads it, and extracts it on the sandbox by default.',
    inputSchema: {
      type: 'object',
      required: ['local_dir'],
      properties: {
        local_dir: { type: 'string', description: 'Local project directory to upload.' },
        remote_dir: {
          type: 'string',
          description:
            'Remote parent directory where project will be extracted (default: /workspace). Final layout: <remote_dir>/<dirname>/',
        },
        workspace_id: {
          type: 'string',
          description:
            'Workspace ID from huaweicloud_sandbox_connect return value. Required - must be passed explicitly when HW_WORKSPACE_ID is not set.',
        },
        username: { type: 'string', description: 'Login username (default: root)' },
        exclude: {
          type: 'array',
          items: { type: 'string' },
          description: 'Patterns to exclude from archive (default: .git, node_modules, __pycache__, .venv)',
        },
        extract: {
          type: 'boolean',
          description: 'Extract tar.gz on sandbox after upload (default: true)',
        },
        timeout_ms: { type: 'number', description: 'Execution timeout in milliseconds (default: 300000)' },
      },
    },
  },
  {
    name: 'huaweicloud_sandbox_check_user',
    description:
      'Check if the current user has completed real-name verification and signed the required agreements. Returns 200 {realnameVerified, agreementSigned} when all good; throws 403 HDKIT_NOT_REALNAME / HDKIT_NOT_AGREEMENT / HDKIT_NOT_REALNAME_AND_AGREEMENT to indicate what is missing. Never signs anything itself.',
    inputSchema: {
      type: 'object',
      properties: {},
    },
  },
  {
    name: 'huaweicloud_sandbox_sign_agreement',
    description:
      "Sign all unsigned or outdated agreements for the current user. Required before huaweicloud_sandbox_connect if check-user returns agreementSigned=false. CRITICAL: only call after the user explicitly consents to signing — never sign agreements on the user's behalf without their explicit request.",
    inputSchema: {
      type: 'object',
      properties: {},
    },
  },
  {
    name: 'huaweicloud_sandbox_connect',
    description:
      'Connect to a sandbox via hdkitservice. One user one instance - reuses existing sandbox if available, otherwise creates a new one. Returns session_id, dev_stage_id, connection_id, and connection_address.',
    inputSchema: {
      type: 'object',
      properties: {
        source: {
          type: 'string',
          description:
            'Source identifier (default: WEB). Options: VSCODE, CLI, WEB, WEBVNC, WEBPTY, WEBIDE, CURSOR, etc.',
        },
        template_id: { type: 'string', description: 'Template ID; overrides server default (only for new sandbox)' },
        flavor_id: { type: 'string', description: 'Flavor ID; overrides server default (only for new sandbox)' },
        env: { type: 'object', description: 'Environment variables to set in the sandbox (only for new sandbox)' },
        git: {
          type: 'object',
          description: 'Git repo config (only for new sandbox)',
          properties: {
            repo_url: { type: 'string', description: 'Git repository URL' },
            repo_branch: { type: 'string', description: 'Git branch' },
            repo_name: { type: 'string', description: 'Repository name' },
            target_path: { type: 'string', description: 'Clone target path in sandbox' },
            open_type: { type: 'string', description: 'Open type' },
          },
        },
      },
    },
  },
  {
    name: 'huaweicloud_sandbox_credentials',
    description:
      'Configure temporary AK/SK for a sandbox via hdkitservice. Injects temporary credentials into the sandbox. The sandbox must be in RUNNING state.',
    inputSchema: {
      type: 'object',
      properties: {
        session_id: { type: 'string', description: 'Session ID from huaweicloud_sandbox_connect' },
        dev_stage_id: { type: 'string', description: 'DevStation environment ID (alternative to session_id)' },
        enable_sts: { type: 'boolean', description: 'Whether to enable STS temporary AK/SK (default: true)' },
      },
    },
  },
];

export async function callTool(name, args = {}) {
  switch (name) {
    case 'huaweicloud_check_cli':
      return runVersionCheck();
    case 'huaweicloud_plan_cli_command':
      return planHcloudCommand(args.args || [], { allowWrites: args.allowWrites === true });
    case 'huaweicloud_run_readonly_command':
      return runHcloud(args.args || [], {
        timeoutMs: args.timeoutMs,
        maxRetries: args.maxRetries,
        cwd: args.cwd,
        stdin: args.stdin,
      });
    case 'huaweicloud_list_operations':
      return listOperations(args.service, { timeoutMs: args.timeoutMs });
    case 'huaweicloud_run_approved_command':
      return runApprovedCommand(args);
    case 'huaweicloud_show_profile_redacted':
      return showProfileRedacted(args.profile);
    case 'huaweicloud_hook_check_command':
      return hookResult(evaluateCommandRisk(args.command || ''));
    case 'huaweicloud_hook_check_artifacts':
      return hookResult(evaluateArtifacts(args.artifacts || []));
    case 'huaweicloud_hook_check_deploy_plan':
      return hookResult(evaluateDeployPlan(args.plan || {}));
    case 'huaweicloud_service_catalog':
      return serviceCatalog(args.intent);
    case 'huaweicloud_search_docs':
      return searchDocs(args.query || '', args.topic || 'all');
    case 'huaweicloud_retrieve_skill':
      return retrieveSkill(args.name || '');
    case 'huaweicloud_list_regions':
      return listRegions();
    case 'huaweicloud_get_regional_availability':
      return getRegionalAvailability(args.service || '', args.region || '');
    case 'huaweicloud_explain_error':
      return explainError(args);
    case 'huaweicloud_search_marketplace':
      return searchMarketplace(args.query || '', args.category || '');
    case 'huaweicloud_get_service_icon':
      return getServiceIcon(args.service || '', args.category || '');
    case 'huaweicloud_setup_obs_config':
      return setupObsConfig(args.profile);
    case 'huaweicloud_auth_status':
      return getAuthStatus(args.target || 'all');
    case 'huaweicloud_auth_sync':
      return syncAuth(args.target || 'all');
    case 'huaweicloud_auth_init':
      if (args.clear) {
        clearRuntimeCredentials();
        return { status: 'cleared', message: 'Runtime credentials cleared. Fallback to env/file.' };
      }
      if (!args.ak || !args.sk) {
        throw new Error('ak and sk are required. Set clear=true to clear runtime credentials.');
      }
      setRuntimeCredentials(args.ak, args.sk, undefined, args.region);
      return { status: 'ok', message: 'Runtime credentials set for this MCP session.' };
    case 'huaweicloud_sandbox_exec_with_session': {
      const sandboxWsId2 = args.workspace_id || getCurrentWorkspaceId();
      if (!sandboxWsId2) {
        throw new Error(
          'workspace_id is required. No sandbox connected — call huaweicloud_sandbox_connect first, ' +
            'or set HW_WORKSPACE_ID environment variable before starting the agent.',
        );
      }
      const sandboxUser2 = args.username || 'root';
      const sandboxTimeout2 = args.timeout_ms || 120000;
      const sandboxResult2 = await execWithSession(sandboxWsId2, args.command, sandboxUser2, sandboxTimeout2);
      return { stdout: sandboxResult2.stdout, exitCode: sandboxResult2.exitCode };
    }
    case 'huaweicloud_sandbox_exec_one_shot': {
      const sandboxWsId3 = args.workspace_id || getCurrentWorkspaceId();
      if (!sandboxWsId3) {
        throw new Error(
          'workspace_id is required. No sandbox connected — call huaweicloud_sandbox_connect first, ' +
            'or set HW_WORKSPACE_ID environment variable before starting the agent.',
        );
      }
      const sandboxUser3 = args.username || 'root';
      const sandboxTimeout3 = args.timeout_ms || 120000;
      const sandboxResult3 = await execOneShot(sandboxWsId3, args.command, sandboxUser3, sandboxTimeout3);
      return { stdout: sandboxResult3.stdout, exitCode: sandboxResult3.exitCode };
    }
    case 'huaweicloud_sandbox_close_session': {
      const sandboxWsId4 = args.workspace_id || getCurrentWorkspaceId();
      if (!sandboxWsId4) {
        throw new Error(
          'workspace_id is required. No sandbox connected — call huaweicloud_sandbox_connect first, ' +
            'or set HW_WORKSPACE_ID environment variable before starting the agent.',
        );
      }
      const sandboxUser4 = args.username || 'root';
      const closed = await closeSession(sandboxWsId4, sandboxUser4);
      return closed ? 'ok' : 'not_connected';
    }
    case 'huaweicloud_sandbox_upload_file': {
      if (!args.local_path || !args.remote_path) {
        throw new Error('local_path and remote_path are required.');
      }
      const sandboxWsId5 = args.workspace_id || getCurrentWorkspaceId();
      if (!sandboxWsId5) {
        throw new Error(
          'workspace_id is required. No sandbox connected — call huaweicloud_sandbox_connect first, ' +
            'or set HW_WORKSPACE_ID environment variable before starting the agent.',
        );
      }
      const sandboxUser5 = args.username || 'root';
      const sandboxTimeout5 = args.timeout_ms || 120000;
      return await uploadFileWithSession(
        sandboxWsId5,
        args.local_path,
        args.remote_path,
        sandboxUser5,
        sandboxTimeout5,
      );
    }
    case 'huaweicloud_sandbox_upload_project': {
      if (!args.local_dir) {
        throw new Error('local_dir is required.');
      }
      const sandboxWsId6 = args.workspace_id || getCurrentWorkspaceId();
      if (!sandboxWsId6) {
        throw new Error(
          'workspace_id is required. No sandbox connected — call huaweicloud_sandbox_connect first, ' +
            'or set HW_WORKSPACE_ID environment variable before starting the agent.',
        );
      }
      const sandboxUser6 = args.username || 'root';
      const sandboxTimeout6 = args.timeout_ms || 120000;
      return await uploadProjectWithSession(
        sandboxWsId6,
        args.local_dir,
        args.remote_dir,
        sandboxUser6,
        sandboxTimeout6,
        {
          exclude: args.exclude,
          extract: args.extract,
        },
      );
    }
    case 'huaweicloud_sandbox_check_user':
      return await hdkitCheckUser();
    case 'huaweicloud_sandbox_sign_agreement':
      return await hdkitSignAgreement();
    case 'huaweicloud_sandbox_connect': {
      const connectResult = await hdkitConnect(args);
      const devStageId = connectResult?.dev_stage_id || connectResult?.devStageId;
      if (devStageId) {
        setWorkspaceId(devStageId);
      }
      return connectResult;
    }
    case 'huaweicloud_sandbox_credentials':
      return await hdkitCredentials(args.session_id, args.dev_stage_id, args.enable_sts !== false);
    default:
      throw new Error(`Unknown tool: ${name}`);
  }
}

function hookResult(result) {
  return {
    ok: result.decision !== 'deny',
    decision: result.decision,
    findings: result.findings,
    nextStep:
      result.decision === 'deny'
        ? 'Revise the command, artifact, or deployment plan before execution.'
        : result.decision === 'warn'
          ? 'Review the warnings with the user before proceeding.'
          : 'No Huawei Cloud hook risk rule matched.',
  };
}

export async function runVersionCheck(options = {}) {
  const result = await runHcloud(['version'], {
    ...options,
    maxRetries: options.maxRetries ?? 0,
  });
  const errorText = result.error || result.stderr || '';
  const isSpawnError = /ENOENT|SPAWN_ERROR/i.test(errorText) || result.code === 'SPAWN_ERROR';
  return {
    installed: result.ok,
    authenticated: result.ok && !/配置文件中不存在配置项|USE_ERROR.*配置/i.test(result.stdout || ''),
    errorCode: isSpawnError ? 'HCLOUD_NOT_FOUND' : undefined,
    output: result.ok ? result.stdout : errorText,
    nextStep: result.ok
      ? 'Use huaweicloud_show_profile_redacted to inspect the active KooCLI profile safely.'
      : isSpawnError
        ? 'hcloud executable not found. Set HCLOUD_BIN to the full hcloud path, or install KooCLI: npx huaweicloud-devkit install-hcloud. Then restart the agent.'
        : 'Install Huawei Cloud KooCLI: npx huaweicloud-devkit install-hcloud. Configure credentials outside the agent conversation.',
    authHint:
      'If hcloud is installed but commands fail with "配置文件中不存在配置项", run `hcloud configure set --cli-access-key=<AK> --cli-secret-key=<SK> --cli-region=<region>` outside agent chat to configure credentials.',
  };
}

async function showProfileRedacted(profile) {
  const args = ['configure', 'show'];
  if (profile) {
    args.push('--cli-profile', String(profile));
  }
  const result = await runHcloud(args, { allowWrites: false, allowCredentialRead: true }).catch((error) => ({
    ok: false,
    blocked: true,
    reason: error.message,
  }));
  if (result.blocked) {
    return {
      ok: false,
      blockedByPolicy: true,
      reason: result.reason,
      safeAlternative:
        'Use huaweicloud_show_profile_redacted so profile output is returned through the redaction pipeline.',
    };
  }
  return {
    ok: result.ok,
    note: result.ok
      ? 'Profile information was returned through the toolkit redaction pipeline.'
      : 'Failed to retrieve profile — hcloud may not be installed or configured.',
    result: redactSecrets(result),
  };
}

async function setupObsConfig(profile) {
  const stored = readGlobalCredentials();
  if (stored?.ak && stored?.sk) {
    try {
      const obs = writeObsConfigFile(stored);
      return {
        ok: true,
        existed: false,
        created: true,
        path: obs.path,
        region: stored.region,
        endpoint: obs.endpoint,
        source: 'global-credentials',
        note: 'OBS credentials synced from the global credential vault. OBS commands (hcloud OBS ls, mb, cp, etc.) should now work.',
      };
    } catch (error) {
      return {
        ok: false,
        error: error.message,
        nextStep: 'Run "npx huaweicloud-devkit auth init" to refresh credentials and region.',
      };
    }
  }

  return setupObsConfigFromHcloud(profile);
}

async function setupObsConfigFromHcloud(profile) {
  const obsConfigPath = join(homedir(), '.obsutilconfig');
  if (existsSync(obsConfigPath)) {
    return {
      ok: true,
      existed: true,
      path: obsConfigPath,
      note: 'OBS config already exists. Delete ~/.obsutilconfig first if you need to re-sync.',
    };
  }

  const args = ['configure', 'show'];
  if (profile) args.push('--cli-profile', String(profile));
  const result = await runHcloud(args, { allowWrites: false, allowCredentialRead: true });

  if (!result.ok) {
    return {
      ok: false,
      error: 'Failed to read hcloud profile.',
      detail: result.error || result.stderr || 'hcloud not installed or not configured',
      nextStep: 'Run "npx huaweicloud-devkit auth init" outside agent chat, then retry.',
    };
  }

  let accessKeyId;
  let secretAccessKey;
  let region;

  try {
    const parsed = typeof result.stdout === 'string' ? JSON.parse(result.stdout) : result.stdout;
    const cred = parsed.currentCredential || {};
    accessKeyId = cred.accessKeyId || cred.ak || cred.access_key || '';
    secretAccessKey = cred.secretAccessKey || cred.sk || cred.secret_key || '';
    region = parsed.currentRegion || parsed.region || '';
  } catch {
    return {
      ok: false,
      error: 'Failed to parse hcloud profile output.',
      detail: 'hcloud configure show returned unexpected format',
    };
  }

  if (!accessKeyId || !secretAccessKey) {
    return {
      ok: false,
      error: 'No credentials found in hcloud profile.',
      nextStep: 'Run "npx huaweicloud-devkit auth init" outside agent chat to set up credentials first.',
    };
  }

  if (!region) {
    return {
      ok: false,
      error: 'No region found in hcloud profile.',
      nextStep: 'Run "hcloud configure set --cli-region=<region>" outside agent chat to set a default region.',
    };
  }

  const endpoint = `https://obs.${region}.myhuaweicloud.com`;
  // Flat key=value format (no [default] section) as written by KooCLI 7.x `hcloud OBS config`.
  const configContent = `endpoint=${endpoint}\nak=${accessKeyId}\nsk=${secretAccessKey}\n`;

  try {
    writeFileSync(obsConfigPath, configContent, { encoding: 'utf8', mode: 0o600 });
  } catch (error) {
    return {
      ok: false,
      error: 'Failed to write OBS config file.',
      detail: error.message,
      path: obsConfigPath,
    };
  }

  return {
    ok: true,
    existed: false,
    created: true,
    path: obsConfigPath,
    region,
    endpoint,
    note: 'OBS credentials synced from hcloud profile. OBS commands (hcloud OBS ls, mb, cp, etc.) should now work.',
  };
}

const SERVICE_EXAMPLES = {
  ECS: { list: 'ECS ListServersDetails', create: 'ECS CreateServers', show: 'IMS GlanceShowImage' },
  VPC: { list: 'VPC ListVpcs', create: 'VPC CreateVpc', show: 'VPC ShowVpc' },
  FUNCTIONGRAPH: {
    list: 'FunctionGraph ListFunctions',
    create: 'FunctionGraph CreateFunction',
    show: 'FunctionGraph ShowFunctionConfig',
  },
  APIG: { list: 'APIG ListInstancesV2', create: 'APIG CreateInstanceV2', show: 'APIG ShowDetailsOfInstanceV2' },
  OBS: { list: 'OBS ls', create: 'OBS mb obs://<bucket>', show: 'OBS stat obs://<bucket>/<key>' },
  RDS: { list: 'RDS ListInstances', create: 'RDS CreateInstance', show: 'RDS ShowInstance' },
  CES: { list: 'CES ListAlarms', create: 'CES CreateAlarm', show: 'CES ListMetrics' },
  GAUSSDB: { list: 'GaussDB ListInstances', create: 'GaussDB CreateInstance', show: 'GaussDB ShowInstance' },
  DDS: { list: 'DDS ListInstances', create: 'DDS CreateInstance', show: 'DDS ShowInstance' },
  DCS: { list: 'DCS ListInstances', create: 'DCS CreateInstance', show: 'DCS ShowInstance' },
};

async function listOperations(service, options = {}) {
  const serviceName = String(service || '').trim();
  if (!/^[A-Za-z][A-Za-z0-9-]{1,63}$/.test(serviceName)) {
    throw new Error('service must be a KooCLI service name such as ECS, VPC, IMS, OBS, RDS, or CDN.');
  }
  const isObs = /^obs$/i.test(serviceName);
  const svc = isObs ? 'obs' : serviceName;
  const args = isObs ? ['obs', 'help'] : [svc, '--help'];
  let result = await runHcloud(args, {
    timeoutMs: options.timeoutMs,
    maxRetries: 0,
  });
  if (!result.ok && !isObs) {
    result = await runHcloud([svc, 'help'], {
      timeoutMs: options.timeoutMs,
      maxRetries: 0,
    });
  }
  return {
    service: serviceName,
    command: isObs ? 'hcloud obs help' : `hcloud ${svc} --help`,
    selectionRule: 'Use this help text to select the exact KooCLI operation name before planning any service command.',
    examples: SERVICE_EXAMPLES[serviceName.toUpperCase()] || {
      note: `No cached examples for ${serviceName}. Use the help text above to discover available operations.`,
    },
    result,
  };
}

async function runApprovedCommand(args = {}) {
  if (args.approvedByUser !== true) {
    throw new Error('approvedByUser must be true after explicit user approval for this exact command.');
  }
  const strictPlan = planHcloudCommand(args.args || [], { allowWrites: false });
  const plan = planHcloudCommand(args.args || [], { allowWrites: true });
  if (String(args.approvedCommand || '') !== plan.command) {
    throw new Error('approvedCommand must exactly match the planned hcloud command.');
  }
  const result = await runHcloud(args.args || [], {
    allowWrites: true,
    timeoutMs: args.timeoutMs,
    maxRetries: args.maxRetries,
    cwd: args.cwd,
    stdin: args.stdin,
  });
  result.approved = true;
  result.plan = strictPlan;
  return result;
}

function serviceCatalog(intent = '') {
  const it = String(intent).toLowerCase();
  const routeMap = [
    {
      keywords: ['ecs', 'server', 'vm', 'instance', 'compute', 'flavor', 'image'],
      skills: ['huawei-ecs'],
      services: ['ECS'],
    },
    {
      keywords: ['vpc', 'subnet', 'network', 'security group', 'eip', 'nat', 'vpn', 'bandwidth'],
      skills: ['huawei-vpc'],
      services: ['VPC', 'EIP'],
    },
    {
      keywords: ['obs', 'bucket', 'storage', 'object', 'static website', 'static site', 'hosting'],
      skills: ['huawei-obs'],
      services: ['OBS'],
    },
    {
      keywords: ['functiongraph', 'serverless', 'function', 'lambda', 'trigger', 'faas'],
      skills: ['huawei-functiongraph'],
      services: ['FunctionGraph'],
    },
    {
      keywords: ['cce', 'kubernetes', 'k8s', 'container', 'cluster', 'node pool', 'swr', 'docker', 'image registry'],
      skills: ['huawei-cce'],
      services: ['CCE', 'SWR'],
    },
    { keywords: ['apig', 'api gateway', 'publish', 'throttle'], skills: ['huawei-apig'], services: ['APIG'] },
    { keywords: ['rds', 'mysql', 'postgresql', 'database', 'db'], skills: ['huawei-rds'], services: ['RDS'] },
    {
      keywords: ['gaussdb', 'distributed', 'sharding', 'opengauss'],
      skills: ['huawei-gaussdb'],
      services: ['GaussDB'],
    },
    {
      keywords: ['iam', 'permission', 'policy', 'role', 'user', 'ak/sk', 'access key', 'agency'],
      skills: ['huawei-iam'],
      services: ['IAM'],
    },
    {
      keywords: ['dew', 'secret', 'kms', 'encrypt', 'decrypt', 'certificate', 'csms'],
      skills: ['huawei-dew'],
      services: ['CSMS', 'KMS'],
    },
    {
      keywords: ['modelarts', 'ai', 'model', 'training', 'inference', 'machine learning'],
      skills: ['huawei-modelarts'],
      services: ['ModelArts'],
    },
    {
      keywords: ['billing', 'cost', 'bill', 'budget', 'expense', 'bss'],
      skills: ['huawei-billing'],
      services: ['BSS'],
    },
    {
      keywords: ['waf', 'aad', 'ddos', 'firewall', 'web protection'],
      skills: ['huawei-waf-aad'],
      services: ['WAF', 'AAD'],
    },
    {
      keywords: ['smn', 'dms', 'notification', 'message', 'kafka', 'rabbitmq'],
      skills: ['huawei-smn-dms'],
      services: ['SMN', 'DMS'],
    },
    {
      keywords: ['ces', 'monitor', 'alarm', 'metric', 'dashboard', 'cloud eye'],
      skills: ['huawei-cloud-eye'],
      services: ['CES'],
    },
    { keywords: ['cts', 'audit', 'trace', 'tracker'], skills: ['huawei-cts'], services: ['CTS'] },
    { keywords: ['cbr', 'backup', 'restore', 'vault', 'snapshot'], skills: ['huawei-cbr'], services: ['CBR'] },
    {
      keywords: ['deployment', 'deploy', 'ci/cd', 'pipeline', 'release'],
      skills: ['huawei-deployment'],
      services: ['CloudDeploy'],
    },
    {
      keywords: [
        'sandbox',
        'devstation',
        'workspace',
        'terminal',
        'preview',
        'hwlink',
        'website',
        'web app',
        'webapp',
        'hosting',
        '网站',
        '网页',
        '静态',
      ],
      skills: ['huawei-sandbox'],
      services: ['Sandbox', 'DevStation'],
    },
    {
      keywords: ['dds', 'dcs', 'mongodb', 'redis', 'memcached', 'cache', 'document db'],
      skills: ['huawei-dds-dcs'],
      services: ['DDS', 'DCS'],
    },
  ];
  const matched = [];
  const tokens = new Set(it.split(/[\s,./-]+/).filter((t) => t.length > 0));
  const cjk = /[\u4e00-\u9fff]/;
  for (const route of routeMap) {
    if (route.keywords.some((kw) => (kw.includes(' ') || cjk.test(kw) ? it.includes(kw) : tokens.has(kw)))) {
      matched.push(route);
    }
  }
  const recommendedSkills = [...new Set(matched.flatMap((r) => r.skills))];
  const recommendedServices = [...new Set(matched.flatMap((r) => r.services))].slice(0, 5);

  // Deployment intent (deploy/host/publish a web app or static website) must never
  // default to a storage/other service — recommend the sandbox first.
  const deploymentIntent = /deploy|host|hosting|publish|website|web app|preview|部署|托管|发布|网站|网页/.test(it);
  if (deploymentIntent && recommendedSkills.includes('huawei-sandbox')) {
    const idx = recommendedSkills.indexOf('huawei-sandbox');
    recommendedSkills.splice(idx, 1);
    recommendedSkills.unshift('huawei-sandbox');
  }

  return {
    intent,
    recommendedSkills: recommendedSkills.length ? recommendedSkills : ['Use huaweicloud-core to route intent.'],
    recommendedServices: recommendedServices.length
      ? recommendedServices
      : ['Run hcloud --help to list available services.'],
    capabilityOrder: [
      'Huawei Cloud Skills for task-specific workflows and examples',
      'KooCLI hcloud for local authenticated operations and quick inspection',
      'Huawei Cloud API documentation for exact request and response contracts',
      'Huawei Cloud SDKs for application code integration',
      'Huawei Cloud MCP when an official or approved server is available',
      'Terraform Provider only when IaC reviewability and repeatability are important',
    ],
    ruleOfThumb: {
      skills: 'Start here when the user describes a scenario or wants a guided workflow.',
      cli: 'Use for local diagnostics, read-only inspection, and commands the user can review.',
      api: 'Use for exact service contract, region endpoint, project_id, pagination, and error codes.',
      sdk: 'Use when writing application code that calls Huawei Cloud services.',
      mcp: 'Prefer approved MCP tools when available because tools can carry structured schemas.',
      terraform: 'Keep low priority in V1; suggest it for reviewed infrastructure changes, not quick diagnosis.',
    },
  };
}

function explainError({ service = 'unknown', errorCode = '', message = '', requestId = '' } = {}) {
  const combined = `${errorCode} ${message}`.toLowerCase();
  const suggestions = [];
  const svc = String(service).toLowerCase();

  const SERVICE_ALIASES = {
    functiongraph: 'FSS',
    fgs: 'FSS',
  };
  const patternKey = SERVICE_ALIASES[svc] || service;

  const hwErrorPatterns = {
    OBS: {
      InvalidAccessKeyId:
        'OBS uses AK/SK directly (not IAM tokens). Verify AK/SK validity, OBS endpoint, and OBS permissions.',
      UserRestricted:
        'This IAM user is restricted from OBS operations (403). Check IAM console → Users → Permissions: grant OBS bucket/object actions, or use an unrestricted account. If the account is a sub-account, the main account may have imposed restrictions.',
    },
    APIG: {
      'APIC.7241': 'The enterprise_project_id is required for enterprise accounts. Add --enterprise_project_id=0.',
      'APIC.7242':
        'The EIP binding method depends on loadbalancer_provider. Use AddIngressEipV2 for elb, AddEipV2 for lvs.',
      'APIC.7256': 'Bandwidth minimum is 5 Mbps. Use --bandwidth_size=5 or higher.',
      'APIC.7310': 'available_zone_ids must use AZ codes (e.g. ap-southeast-3a), NOT UUIDs from ListAvailableZonesV2.',
    },
    FSS: {
      'FSS.0403':
        'Missing FunctionGraph IAM permissions. Attach FunctionGraph FullAccess role or grant specific actions.',
      'FSS.1078': '--code_filename is filename-only (no path). cd to the file directory before running the command.',
      'FSS.1417':
        'event_data field validation failed. Check parameter format: use dotted key=value, verify required hidden-optional fields.',
    },
    VPC: {
      'VPC.0301': 'Bandwidth name is required for PER type EIPs, even though --help marks it optional.',
    },
    APIGW: {
      'APIGW.0802':
        'The current IAM user has no permissions in the requested region. Go to IAM console → Users → Permissions → add the target region, or switch to a different region.',
    },
  };
  if (hwErrorPatterns[patternKey] && hwErrorPatterns[patternKey][errorCode]) {
    suggestions.push(hwErrorPatterns[patternKey][errorCode]);
  }
  const svcPatterns = hwErrorPatterns[patternKey] || {};
  for (const [code, tip] of Object.entries(svcPatterns)) {
    if (errorCode && code.includes(errorCode)) {
      if (!suggestions.includes(tip)) suggestions.push(tip);
    }
  }

  if (/auth|token|credential|ak|sk|401|403|unauthorized|forbidden|Incorrect IAM/i.test(combined)) {
    if (svc === 'obs') {
      suggestions.push(
        'OBS uses AK/SK directly, not IAM tokens. Verify AK/SK validity and OBS bucket permissions via hcloud configure list or the Huawei Cloud console.',
      );
    } else {
      suggestions.push('Check KooCLI profile, region, project_id, and IAM permissions without printing secrets.');
    }
  }
  if (/APIGW\.(\d+)/i.test(errorCode)) {
    suggestions.push(
      'APIGW.' +
        (errorCode.match(/APIGW\.(\d+)/i) || [])[1] +
        ': API Gateway layer error. ' +
        (errorCode === 'APIGW.0802'
          ? 'IAM user has no region permissions — check IAM console → User → Permissions → add target region.'
          : 'Verify the API request, region endpoint, and IAM permissions.'),
    );
  }
  if (/region|endpoint|project/i.test(combined)) {
    suggestions.push('Confirm the service endpoint, region, and project_id match the target resource.');
  }
  if (/quota|limit|insufficient|reach the limit/i.test(combined)) {
    suggestions.push(
      'Check quota and resource limits before retrying a create or scale operation. Consider switching accounts or requesting a quota increase.',
    );
  }
  if (/not.?found|404/i.test(combined)) {
    if (/list.?regions/i.test(svc)) {
      suggestions.push('Use hcloud IAM KeystoneListRegions (not list-regions).');
    } else {
      suggestions.push('List resources in the same region/project and verify the resource identifier.');
    }
  }
  if (!suggestions.length) {
    suggestions.push(
      'Collect service name, operation, region, project_id, request_id, and the full redacted error message.',
    );
  }

  if (requestId) {
    suggestions.push('Provide the Request ID (' + requestId + ') when contacting Huawei Cloud support.');
  }

  const uniqueSuggestions = suggestions.filter((s, i, arr) => {
    return !arr.slice(0, i).some((prev) => prev.substring(0, 50).toLowerCase() === s.substring(0, 50).toLowerCase());
  });

  return {
    service,
    errorCode,
    requestId,
    suggestions: uniqueSuggestions,
  };
}

async function searchDocs(query, topic = 'all') {
  const q = String(query || '').toLowerCase();
  const tokens = q.split(/\s+/).filter((t) => t.length > 0);
  const results = [];
  try {
    if (existsSync(SKILLS_ROOT)) {
      const dirs = listSkillDirs(SKILLS_ROOT);
      for (const dir of dirs) {
        const skillPath = join(SKILLS_ROOT, dir, 'SKILL.md');
        if (!existsSync(skillPath)) continue;
        const content = readFileSync(skillPath, 'utf8');
        const frontmatter = content.match(/^---\r?\n([\s\S]*?)\r?\n---/);
        let description = '';
        let name = dir;
        if (frontmatter) {
          const fm = frontmatter[1];
          const nameMatch = fm.match(/^name:\s*(.+)$/m);
          if (nameMatch) name = nameMatch[1].trim();
          const descMatch = fm.match(/^description:\s*(.+)$/m);
          if (descMatch) description = descMatch[1].trim();
        }
        if (topic !== 'all') {
          const topicLower = topic.toLowerCase();
          if (!name.toLowerCase().includes(topicLower) && !description.toLowerCase().includes(topicLower)) continue;
        }
        const descLower = description.toLowerCase();
        const nameLower = name.toLowerCase();
        const contentLower = content.toLowerCase();
        const relevance = tokens.reduce((score, token) => {
          return (
            score +
            (descLower.includes(token) ? 3 : 0) +
            (nameLower.includes(token) ? 2 : 0) +
            (contentLower.includes(token) ? 1 : 0)
          );
        }, 0);
        if (relevance > 0) {
          results.push({
            source: 'skills/' + dir + '/SKILL.md',
            name,
            snippet: description.substring(0, 200),
            relevance,
          });
        }
      }
    }
  } catch (error) {
    return { ok: false, error: error.message, results: [] };
  }
  results.sort((a, b) => b.relevance - a.relevance);
  return { ok: true, query: q, topic, count: results.length, results: results.slice(0, 10) };
}

async function retrieveSkill(name) {
  const skillName = String(name || '').trim();
  if (!skillName) return { ok: false, error: 'Skill name is required.' };
  const skillPath = join(SKILLS_ROOT, skillName, 'SKILL.md');
  if (!existsSync(skillPath)) {
    const dirs = listSkillDirs(SKILLS_ROOT);
    return { ok: false, error: 'Skill "' + skillName + '" not found. Available: ' + dirs.join(', ') };
  }
  const content = readFileSync(skillPath, 'utf8');
  const references = [];
  const refDir = join(SKILLS_ROOT, skillName, 'references');
  if (existsSync(refDir)) {
    readdirSync(refDir).forEach((f) => {
      const refPath = join(refDir, f);
      references.push({ filename: f, content: readFileSync(refPath, 'utf8').substring(0, 4000) });
    });
  }
  const frontmatter = content.match(/^---\r?\n([\s\S]*?)\r?\n---/);
  let version = 1,
    description = '';
  if (frontmatter) {
    const fm = frontmatter[1];
    const vm = fm.match(/^version:\s*(.+)$/m);
    if (vm) version = vm[1].trim();
    const dm = fm.match(/^description:\s*(.+)$/m);
    if (dm) description = dm[1].trim();
  }
  return { ok: true, name: skillName, version, description, content, references };
}

async function listRegions() {
  const result = await runHcloud(['IAM', 'KeystoneListRegions'], { timeoutMs: 30000, maxRetries: 0 }).catch(
    (error) => ({
      ok: false,
      error: error.message,
    }),
  );
  if (!result.ok) {
    return {
      ok: false,
      error: result.error || 'Failed to list regions.',
      fallback: 'Check https://developer.huaweicloud.com/endpoint for available regions.',
      staticRegions: [
        { id: 'cn-north-1', description: '华北-北京一' },
        { id: 'cn-north-4', description: '华北-北京四' },
        { id: 'cn-east-3', description: '华东-上海一' },
        { id: 'cn-east-2', description: '华东-上海二' },
        { id: 'cn-south-1', description: '华南-广州' },
        { id: 'ap-southeast-1', description: '香港' },
        { id: 'ap-southeast-3', description: '新加坡' },
        { id: 'ap-southeast-2', description: '曼谷' },
        { id: 'af-south-1', description: '约翰内斯堡' },
      ],
      note: 'hcloud unavailable. Showing static region list. For the complete list, visit the fallback URL.',
    };
  }
  let regions;
  try {
    const parsed = typeof result.stdout === 'string' ? JSON.parse(result.stdout) : result.stdout;
    const rawRegions = parsed.regions || [];
    regions = rawRegions.map((r) => ({
      id: r.id,
      description: r.description || r.names,
      type: r.type,
      locales: r.locales,
    }));
  } catch {
    regions = [{ raw: String(result.stdout).substring(0, 1000) }];
  }
  regions.sort((a, b) => String(a.id || '').localeCompare(String(b.id || '')));
  return { ok: true, count: regions.length, regions };
}

async function getRegionalAvailability(service, region) {
  const svc = String(service || '')
    .toLowerCase()
    .trim();
  const reg = String(region || '')
    .toLowerCase()
    .trim();
  if (!svc || !reg) return { ok: false, error: 'Both service and region are required.' };
  const known = {
    ecs: [
      'cn-south-1',
      'cn-north-4',
      'cn-north-1',
      'cn-east-3',
      'cn-east-2',
      'ap-southeast-3',
      'ap-southeast-2',
      'ap-southeast-1',
      'ap-southeast-4',
      'af-south-1',
      'tr-west-1',
      'sa-brazil-1',
      'la-north-2',
      'na-mexico-1',
      'me-east-1',
    ],
    obs: [
      'cn-south-1',
      'cn-north-4',
      'cn-north-1',
      'cn-east-3',
      'cn-east-2',
      'ap-southeast-3',
      'ap-southeast-2',
      'ap-southeast-1',
      'af-south-1',
    ],
    vpc: [
      'cn-south-1',
      'cn-north-4',
      'cn-north-1',
      'cn-east-3',
      'cn-east-2',
      'ap-southeast-3',
      'ap-southeast-2',
      'ap-southeast-1',
      'ap-southeast-4',
      'af-south-1',
      'tr-west-1',
      'sa-brazil-1',
      'la-north-2',
      'me-east-1',
    ],
    iam: ['global'],
    rds: [
      'cn-south-1',
      'cn-north-4',
      'cn-north-1',
      'cn-east-3',
      'cn-east-2',
      'ap-southeast-3',
      'ap-southeast-2',
      'ap-southeast-1',
    ],
    gaussdb: ['cn-south-1', 'cn-north-4', 'cn-east-3'],
    cce: [
      'cn-south-1',
      'cn-north-4',
      'cn-north-1',
      'cn-east-3',
      'cn-east-2',
      'ap-southeast-3',
      'ap-southeast-2',
      'ap-southeast-1',
    ],
    modelarts: ['cn-south-1', 'cn-north-4', 'cn-east-3'],
    functiongraph: [
      'cn-south-1',
      'cn-north-4',
      'cn-north-1',
      'cn-east-3',
      'cn-east-2',
      'ap-southeast-3',
      'ap-southeast-2',
      'ap-southeast-1',
    ],
    dew: ['cn-south-1', 'cn-north-4', 'cn-east-3', 'ap-southeast-3'],
    smn: ['cn-south-1', 'cn-north-4', 'cn-east-3', 'ap-southeast-3'],
    ces: ['cn-south-1', 'cn-north-4', 'cn-east-3', 'ap-southeast-3'],
    cts: ['cn-south-1', 'cn-north-4', 'cn-east-3', 'ap-southeast-3'],
    apig: ['cn-south-1', 'cn-north-4', 'cn-east-3', 'ap-southeast-3'],
    cbr: ['cn-south-1', 'cn-north-4', 'cn-east-3', 'ap-southeast-3'],
    dds: ['cn-south-1', 'cn-north-4', 'cn-east-3', 'ap-southeast-3'],
    dcs: ['cn-south-1', 'cn-north-4', 'cn-east-3', 'ap-southeast-3'],
  };
  if (!known[svc])
    return {
      ok: false,
      service: svc,
      region: reg,
      available: false,
      note:
        'Service ' +
        svc +
        ' is not in the regional availability cache. Run hcloud ' +
        svc.toUpperCase() +
        ' --help to verify, or check https://developer.huaweicloud.com/endpoint.',
    };
  const available = known[svc].includes(reg) || known[svc].includes('global');
  return {
    ok: true,
    service: svc,
    region: reg,
    available,
    note: available
      ? svc + ' is available in ' + reg + '.'
      : svc +
        ' availability in ' +
        reg +
        ' could not be confirmed. Verify at https://developer.huaweicloud.com/endpoint.',
    sourcedFrom: 'static cache, update via npm package upgrade',
    disclaimer:
      'This result reflects service-level availability only. It does NOT guarantee that your IAM user has permissions in this region. Account-level restrictions (e.g., APIGW.0802) may block actual API calls even when the service is available.',
  };
}

export function classifyRawCommand(command) {
  return classifyTextCommand(command);
}

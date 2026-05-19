#!/usr/bin/env node
// SmallCode — AI coding agent for small LLMs
// Entry point: parses args, boots the TUI or runs in non-interactive mode
//
// This is the bootstrap that loads until the Marrowscript runtime is ready.
// It provides the same interface the compiled .marrow output would.

const { spawn } = require('child_process');
const path = require('path');
const fs = require('fs');

// Load .env file (checks multiple locations, first found wins)
(function loadDotenv() {
  const os = require('os');
  const envPaths = [
    path.join(process.cwd(), '.env'),                          // project root
    path.join(process.cwd(), '.smallcode', '.env'),            // .smallcode dir
    path.join(os.homedir(), '.config', 'smallcode', '.env'),   // global config
    path.join(os.homedir(), '.smallcode', '.env'),             // global alt
  ];

  for (const envPath of envPaths) {
    if (!fs.existsSync(envPath)) continue;
    try {
      const content = fs.readFileSync(envPath, 'utf-8');
      for (const line of content.split('\n')) {
        const trimmed = line.trim();
        if (!trimmed || trimmed.startsWith('#')) continue;
        const eqIdx = trimmed.indexOf('=');
        if (eqIdx === -1) continue;
        const key = trimmed.slice(0, eqIdx).trim();
        let value = trimmed.slice(eqIdx + 1).trim();
        // Remove surrounding quotes
        if ((value.startsWith('"') && value.endsWith('"')) || (value.startsWith("'") && value.endsWith("'"))) {
          value = value.slice(1, -1);
        }
        // Don't override existing env vars
        if (!process.env[key]) process.env[key] = value;
      }
      break; // Use first found .env file
    } catch {}
  }
})();
const readline = require('readline');
const os = require('os');
const tui = require('./tui');
const chalk = tui.chalk;
let McpMemoryStore;
try {
  McpMemoryStore = require('budget-aware-mcp/dist/memory/store.js').MemoryStore;
} catch {
  McpMemoryStore = null;
}
const { ToolScorer, checkAndEnforceHardFail, classifyTask } = require('./governor');
const { EscalationEngine } = require('./escalation');
const { PluginLoader } = require('../src/plugins/loader');
const { SkillManager } = require('../src/plugins/skills');
const { SessionStore } = require('../src/session/persistence');
const { resolveReferences, formatReferencesForPrompt } = require('../src/session/references');
const { TokenTracker } = require('../src/session/tokens');
const { UndoStack } = require('../src/session/undo');
const { shouldInjectGitContext, getGitDiffContext } = require('../src/session/git_context');
const { routeModel } = require('../src/model/router');

// Initialize structured memory (budget-aware-mcp's SQLite + FTS5 store, falls back to JSON)
let memoryStore;
try {
  if (McpMemoryStore) {
    memoryStore = new McpMemoryStore(process.cwd());
  } else {
    throw new Error('budget-aware-mcp not available');
  }
} catch {
  const { MemoryStore } = require('./memory');
  memoryStore = new MemoryStore(process.cwd());
}

// Initialize governor (tool scoring + verification)
const toolScorer = new ToolScorer();
let currentTaskType = 'coding';

// Initialize escalation engine (lazy — resolves config at boot)
let escalationEngine = null; // created after config loads

// Initialize plugin + skill systems
let pluginLoader = null;
let skillManager = null;

// Session persistence + token tracking
let sessionStore = null;
let tokenTracker = null;

// Fullscreen TUI reference for streaming (set when fullscreen mode is active)
let _fullscreenRef = null;

const VERSION = '0.4.14';
const LOGO = `
  ⚡ SmallCode v${VERSION}
  AI coding agent for small LLMs
`;

// ─── Built-in MCP: budget-aware-mcp for code intelligence ───────────────────

let mcpProcess = null;

function startCodeGraphMCP() {
  // Try to find budget-aware-mcp
  const possiblePaths = [
    path.join(__dirname, '..', '..', 'code-graph-mcp', 'dist', 'index.js'),
    path.join(__dirname, '..', 'node_modules', 'budget-aware-mcp', 'dist', 'index.js'),
    path.join(__dirname, '..', 'node_modules', '.package-lock.json'), // trigger re-check
  ];

  let mcpPath = null;
  for (const p of possiblePaths) {
    if (fs.existsSync(p)) { mcpPath = p; break; }
  }

  // Also check if the linked package has a dist/index.js
  if (!mcpPath) {
    const linkedPath = path.join(__dirname, '..', 'node_modules', 'budget-aware-mcp');
    if (fs.existsSync(linkedPath)) {
      const realPath = fs.realpathSync(linkedPath);
      const candidate = path.join(realPath, 'dist', 'index.js');
      if (fs.existsSync(candidate)) mcpPath = candidate;
    }
  }

  // Try global install
  if (!mcpPath) {
    try {
      const { execSync } = require('child_process');
      const globalPath = execSync('npm root -g', { encoding: 'utf-8' }).trim();
      const gp = path.join(globalPath, 'budget-aware-mcp', 'dist', 'index.js');
      if (fs.existsSync(gp)) mcpPath = gp;
    } catch {}
  }

  if (!mcpPath) {
    return null;
  }

  // Start the MCP server as a child process (stdio transport)
  const child = spawn('node', [mcpPath], {
    stdio: ['pipe', 'pipe', 'pipe'],
    cwd: process.cwd(),
  });

  child.on('error', () => {});
  child.on('exit', () => { mcpProcess = null; });

  mcpProcess = child;
  return child;
}

let mcpRequestId = 1;
async function mcpCall(method, params = {}) {
  if (!mcpProcess) return null;

  return new Promise((resolve) => {
    if (!mcpProcess || !mcpProcess.stdout || !mcpProcess.stdin) { resolve(null); return; }

    const id = mcpRequestId++;
    const request = JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n';

    let buffer = '';
    const onData = (chunk) => {
      buffer += chunk.toString();
      const lines = buffer.split('\n');
      buffer = lines.pop() || '';
      for (const line of lines) {
        if (!line.trim()) continue;
        try {
          const resp = JSON.parse(line);
          if (resp.id === id) {
            mcpProcess.stdout.off('data', onData);
            resolve(resp.result || null);
          }
        } catch {}
      }
    };

    mcpProcess.stdout.on('data', onData);
    mcpProcess.stdin.write(request);

    // Timeout after 5s
    setTimeout(() => {
      if (mcpProcess) mcpProcess.stdout.off('data', onData);
      resolve(null);
    }, 5000);
  });
}

async function initCodeGraph() {
  const child = startCodeGraphMCP();
  if (!child) return false;

  // Initialize
  const initResult = await mcpCall('initialize', {
    protocolVersion: '2024-11-05',
    capabilities: {},
    clientInfo: { name: 'smallcode', version: VERSION },
  });

  if (!initResult) {
    mcpProcess = null;
    return false;
  }

  // Check what's already indexed — don't re-index if repos exist
  const listResult = await mcpCall('tools/call', {
    name: 'list_repos',
    arguments: {},
  });

  let alreadyIndexed = 0;
  if (listResult && listResult.content) {
    try {
      const data = JSON.parse(listResult.content[0]?.text || '{}');
      alreadyIndexed = data.total || 0;
    } catch {}
  }

  if (alreadyIndexed > 0) {
    // Already have indexed repos — skip re-indexing
    return true;
  }

  // No repos indexed yet — discover sub-projects and index each
  const cwd = process.cwd();
  const subProjects = [];
  try {
    const entries = fs.readdirSync(cwd, { withFileTypes: true });
    for (const entry of entries) {
      if (!entry.isDirectory()) continue;
      if (entry.name.startsWith('.') || entry.name === 'node_modules' || entry.name === 'venv') continue;
      const subPath = path.join(cwd, entry.name);
      // Check if it has source files (package.json, Cargo.toml, go.mod, src/, etc.)
      const markers = ['package.json', 'Cargo.toml', 'go.mod', 'pyproject.toml', 'src'];
      const hasMarker = markers.some(m => fs.existsSync(path.join(subPath, m)));
      if (hasMarker) {
        subProjects.push({ path: subPath, name: entry.name });
      }
    }
  } catch {}

  if (subProjects.length > 0) {
    // Index each sub-project
    for (const proj of subProjects.slice(0, 8)) { // Cap at 8 to avoid long init
      await mcpCall('tools/call', {
        name: 'index_repo',
        arguments: { path: proj.path, name: proj.name },
      });
    }
  } else {
    // Single project — index cwd directly
    await mcpCall('tools/call', {
      name: 'index_repo',
      arguments: { path: cwd, name: path.basename(cwd) },
    });
  }

  return true;
}

// ─── Arg Parsing ─────────────────────────────────────────────────────────────

const args = process.argv.slice(2);
const flags = {};
const positional = [];

for (let i = 0; i < args.length; i++) {
  const arg = args[i];
  if (arg === '-h' || arg === '--help') flags.help = true;
  else if (arg === '--version') flags.version = true;
  else if (arg === '-V' || arg === '--verbose') flags.verbose = true;
  else if (arg === '-v') flags.version = true;
  else if (arg === '-r' || arg === '--resume') flags.resume = true;
  else if (arg === '--mcp') flags.mcp = true;
  else if (arg === '--acp') flags.acp = true;
  else if (arg === '--non-interactive') flags.nonInteractive = true;
  else if (arg === '--classic') flags.classic = true;
  else if (arg === '-m' || arg === '--model') { flags.model = args[++i]; }
  else if (arg === '-p' || arg === '--provider') { flags.provider = args[++i]; }
  else if (arg === '--eval') { flags.eval = args[++i] || 'classify_accuracy'; }
  else if (arg === '--trace') { flags.trace = args[++i]; }
  else positional.push(arg);
}

// ─── Quick exits ─────────────────────────────────────────────────────────────

if (flags.version) {
  console.log(`smallcode v${VERSION}`);
  process.exit(0);
}

if (flags.help) {
  console.log(`${LOGO}
USAGE:
  smallcode [OPTIONS] [PROMPT]

OPTIONS:
  -h, --help              Show this help
  -v, --version           Show version
  -V, --verbose           Verbose output (show tool I/O)
  -m, --model <NAME>      Model to use (default: qwen2.5-coder:14b)
  -p, --provider <NAME>   Provider (ollama, openai, anthropic, llamacpp)
  -r, --resume            Resume last active session
  --non-interactive       Run single prompt, no TUI
  --classic             Use classic readline TUI (no alternate screen)
  --mcp                   Run as MCP server (JSON-RPC over stdio)
  --eval <SUITE>          Run prompt evaluation suite
  --trace <ID>            Replay a recorded trace

COMMANDS (in TUI):
  /quit, /q       Exit
  /clear          Reset conversation
  /stats          Session statistics
  /memory         Show working memory
  /plan           Show task plan
  /undo           Revert last edit
  /sessions       List saved sessions
  /save           Save session
  /eval           Run prompt evaluation
  /budget         Show token budget
  /help           All commands

EXAMPLES:
  smallcode                                Start interactive TUI
  smallcode "fix the bug in main.ts"       Single prompt
  smallcode -m qwen3:8b                    Use specific model
  smallcode --resume                       Continue last session
  smallcode --mcp                          Start as MCP server
  echo "refactor" | smallcode --non-interactive
`);
  process.exit(0);
}

// ─── Config ──────────────────────────────────────────────────────────────────

function loadConfig() {
  const env = process.env;

  const config = {
    model: {
      provider: env.SMALLCODE_PROVIDER || 'openai',
      name: env.SMALLCODE_MODEL || '',
      baseUrl: env.SMALLCODE_BASE_URL || (env.OLLAMA_HOST ? (env.OLLAMA_HOST + '/v1') : 'http://localhost:1234/v1'),
    },
    context: {
      max_budget_pct: parseInt(env.SMALLCODE_CONTEXT_BUDGET) || 70,
      detected_window: parseInt(env.SMALLCODE_CONTEXT_WINDOW) || 0, // 0 = auto-detect from endpoint
      working_memory_tokens: 500,
      summary_threshold: 200,
    },
    tools: {
      bash_timeout: parseInt(env.SMALLCODE_BASH_TIMEOUT) || 30,
    },
    tui: {
      show_token_usage: true,
      auto_approve: env.SMALLCODE_AUTO_APPROVE === 'true',
      theme: env.SMALLCODE_THEME || 'dark',
    },
    escalation: {
      enabled: true,
      max_per_session: parseInt(env.SMALLCODE_ESCALATION_MAX) || 5,
      confirm: env.SMALLCODE_ESCALATION_CONFIRM !== 'false',
      provider: null,
      api_key: null,
      model: env.SMALLCODE_ESCALATION_MODEL || null,
    },
    git: {
      auto_commit: env.SMALLCODE_AUTO_COMMIT === 'true',
    },
  };

  // Multi-model routing (optional)
  if (env.SMALLCODE_MODEL_FAST || env.SMALLCODE_MODEL_STRONG) {
    config.models = {
      fast: env.SMALLCODE_MODEL_FAST || config.model.name,
      default: env.SMALLCODE_MODEL_DEFAULT || config.model.name,
      strong: env.SMALLCODE_MODEL_STRONG || config.model.name,
    };
  }

  // Legacy: still check smallcode.toml / config.toml for backwards compatibility
  const tomlPaths = [
    path.join(process.cwd(), 'smallcode.toml'),
    path.join(process.cwd(), '.smallcode', 'config.toml'),
    path.join(os.homedir(), '.config', 'smallcode', 'config.toml'),
  ];
  for (const tomlPath of tomlPaths) {
    if (fs.existsSync(tomlPath) && !config.model.name) {
      try {
        const content = fs.readFileSync(tomlPath, 'utf-8');
        const lines = content.split('\n');
        for (const line of lines) {
          const m = line.match(/^name\s*=\s*"?([^"#]+)"?/);
          if (m) config.model.name = m[1].trim();
          const b = line.match(/^(?:baseUrl|base_url)\s*=\s*"?([^"#]+)"?/);
          if (b) config.model.baseUrl = b[1].trim();
          const p = line.match(/^provider\s*=\s*"?([^"#]+)"?/);
          if (p) config.model.provider = p[1].trim();
        }
        break;
      } catch {}
    }
  }

  // CLI flags override everything
  if (flags.model) config.model.name = flags.model;
  if (flags.provider) config.model.provider = flags.provider;
  if (flags.classic) config.tui.classic = true;

  return config;
}



// ─── Ollama Check ────────────────────────────────────────────────────────────

async function checkOllama(config) {
  const baseUrl = config.model.baseUrl || process.env.OLLAMA_HOST || 'http://localhost:11434';

  // LM Studio / OpenAI-compatible endpoint
  if (config.model.provider === 'openai' || baseUrl.includes('/v1')) {
    try {
      // Include auth header if API key is available (needed for remote/authenticated endpoints)
      const headers = {};
      const apiKey = process.env.OPENAI_API_KEY || process.env.ANTHROPIC_API_KEY || process.env.DEEPSEEK_API_KEY || config.model.apiKey;
      if (apiKey) {
        headers['Authorization'] = `Bearer ${apiKey}`;
      }
      const response = await fetch(`${baseUrl}/models`, { headers });
      if (!response.ok) {
        console.log(`  ⚠ Cannot reach endpoint at ${baseUrl}`);
        console.log(`  Check that your model server is running and accessible.`);
        if (response.status === 401 || response.status === 403) {
          console.log(`  Got ${response.status} — set OPENAI_API_KEY in .env if your server requires auth.`);
        }
        return false;
      }
      const data = await response.json();
      const models = data.data || [];
      if (models.length > 0) {
        console.log(`  Connected: ${baseUrl}`);
        console.log(`  Model: ${config.model.name}`);
        // Auto-detect context window from model metadata
        const activeModel = models.find(m => (m.id || m.name || '').includes(config.model.name)) || models[0];
        if (activeModel && activeModel.context_length) {
          config.context.detected_window = activeModel.context_length;
          console.log(`  Context: ${activeModel.context_length} tokens`);
        }
      }
      return true;
    } catch (e) {
      console.log(`  ⚠ Cannot reach endpoint at ${baseUrl}`);
      console.log(`  Check that your model server is running and the URL is correct.`);
      return false;
    }
  }

  // Ollama endpoint
  const host = process.env.OLLAMA_HOST || 'http://localhost:11434';
  try {
    const response = await fetch(`${host}/api/tags`);
    if (!response.ok) return false;
    const data = await response.json();
    const models = data.models || [];
    const hasModel = models.some(m => m.name.includes(config.model.name.split(':')[0]));
    if (!hasModel) {
      console.log(`  ⚠ Model "${config.model.name}" not found in Ollama.`);
      console.log(`  Run: ollama pull ${config.model.name}`);
      return false;
    }
    return true;
  } catch {
    console.log('  ⚠ Ollama not running. Start it with: ollama serve');
    return false;
  }
}

// ─── TUI ─────────────────────────────────────────────────────────────────────

// Conversation history for multi-turn
const conversationHistory = [];
const improvementAttempts = {}; // filePath → attempt count

async function runTUI(config) {
  const createCommandHandler = require('./commands');
  const handleCmd = createCommandHandler(config, conversationHistory, improvementAttempts, runAgentLoop, runValidation, MAX_IMPROVE_ITERATIONS, memoryStore, escalationEngine);

  const ok = await checkOllama(config);
  if (!ok && config.model.provider === 'ollama') {
    process.exit(1);
  }

  // Start built-in code graph MCP
  let graphOk = false;
  process.stdout.write(chalk.gray('  Code graph: '));
  graphOk = await initCodeGraph();
  if (graphOk) {
    console.log(chalk.green('✓ indexed'));
  } else {
    console.log(chalk.gray('disabled'));
  }

  // ─── FULLSCREEN TUI (default) ─────────────────────────────────────────
  if (!flags.classic) {
    const { FullScreenTUI } = require('../src/tui/fullscreen.js');

    const screen = new FullScreenTUI({
      model: config.model.name,
      theme: config.tui?.theme || 'dark',
      showToolPanel: (process.stdout.columns || 80) > 120,
      onSubmit: async (input) => {
        screen.setStreaming(true);
        await runAgentLoop(input, config);
        screen.setStreaming(false);
        // Update token counter in status bar
        if (tokenTracker) screen.setTokenInfo(tokenTracker.formatShort());
        // Push recent assistant message to screen
        const lastMsg = conversationHistory.filter(m => m.role === 'assistant').pop();
        if (lastMsg && lastMsg.content) {
          screen.addChat('assistant', lastMsg.content);
        }
      },
      onCommand: async (cmd) => {
        if (cmd === '/quit' || cmd === '/q' || cmd === '/exit') {
          if (sessionStore) sessionStore.save(conversationHistory, { tokens: tokenTracker ? tokenTracker.stats() : undefined });
          screen.leave();
          if (mcpProcess) { mcpProcess.kill(); mcpProcess = null; }
          process.exit(0);
        }
        // Capture command output by temporarily redirecting stdout + console.log
        const origWrite = process.stdout.write.bind(process.stdout);
        const origConsoleLog = console.log;
        let captured = '';
        process.stdout.write = (chunk) => { captured += chunk.toString(); return true; };
        console.log = (...args) => { captured += args.join(' ') + '\n'; };
        // Create a mock rl for command handler
        const mockRl = { prompt: () => {}, close: () => { screen.leave(); process.exit(0); } };
        try {
          await handleCmd(cmd, mockRl);
        } catch (e) {
          captured += `Error: ${e.message}\n`;
        }
        process.stdout.write = origWrite;
        console.log = origConsoleLog;
        if (captured.trim()) {
          // Strip ANSI codes for clean display in chat panel
          const clean = captured.replace(/\x1b\[[0-9;]*m/g, '').trim();
          screen.addChat('system', clean);
        }
        screen.render();
      },
      onExit: () => {
        // Save session before exit
        if (sessionStore) {
          sessionStore.save(conversationHistory, { tokens: tokenTracker ? tokenTracker.stats() : undefined });
        }
        if (mcpProcess) { mcpProcess.kill(); mcpProcess = null; }
        process.exit(0);
      },
    });

    // Enter fullscreen FIRST (captures real stdout.write as _rawWrite)
    screen.enter();
    _fullscreenRef = screen;

    // Track current tool name for pairing stdout.write (tool start) with console.log (result)
    let _currentToolName = '';

    // Override console.log to push tool output to the screen with detail
    const origLog = console.log;
    console.log = (...args) => {
      const text = args.map(a => typeof a === 'string' ? a : JSON.stringify(a)).join(' ');
      const clean = text.replace(/\x1b\[[0-9;]*m/g, '').trim();
      if (!clean) return;
      // Skip turn summaries unless verbose
      if (clean.startsWith('───') && !flags.verbose) return;
      // Pair with current tool name for rich display
      if (_currentToolName) {
        const isError = clean.startsWith('✗') || clean.includes('Exit code') || clean.includes('Timed out');
        screen.addTool(_currentToolName, isError ? 'err' : 'ok', clean);
        _currentToolName = '';
      } else {
        screen.addTool('', 'ok', clean);
      }
    };

    // Override process.stdout.write — capture tool name from tui.toolStart calls
    const origStdoutWrite = process.stdout.write.bind(process.stdout);
    process.stdout.write = (chunk) => {
      const text = chunk.toString().replace(/\x1b\[[0-9;]*m/g, '').trim();
      if (!text) return true;
      // tui.toolStart outputs "  ⚙ toolName " — extract the tool name
      const toolMatch = text.match(/^⚙\s*(\S+)/);
      if (toolMatch) {
        _currentToolName = toolMatch[1];
      }
      return true;
    };

    return; // Event loop takes over via raw stdin
  }

  // ─── CLASSIC TUI (--classic flag) ─────────────────────────────────────
  console.log(tui.renderWelcome(config, graphOk));

  const rl = readline.createInterface({
    input: process.stdin,
    output: process.stdout,
    prompt: chalk.cyan('› '),
  });

  rl.prompt();

  rl.on('line', async (line) => {
    const input = line.trim();
    if (!input) { rl.prompt(); return; }

    if (input.startsWith('/')) {
      await handleCmd(input, rl);
      return;
    }

    console.log('');
    await runAgentLoop(input, config);
    console.log('');
    console.log(tui.renderStatus(config, conversationHistory.length));
    rl.prompt();
  });

  rl.on('close', () => {
    if (mcpProcess) { mcpProcess.kill(); mcpProcess = null; }
    console.log(chalk.gray('\n  Goodbye!\n'));
    process.exit(0);
  });
}

// ─── Model Communication ─────────────────────────────────────────────────────

// Tool definitions sent to the model
const TOOLS = [
  {
    type: 'function',
    function: {
      name: 'list_projects',
      description: 'List all indexed projects/repos in the workspace with stats: file count, symbol count, lines of code, languages. Use this FIRST when asked about "the projects", "the codebase", or "what\'s in this workspace".',
      parameters: { type: 'object', properties: {} },
    }
  },
  {
    type: 'function',
    function: {
      name: 'graph_search',
      description: 'Search the code graph for a symbol, function, or class name. Returns connected code with context. Use for "how does X work" or "find the auth logic" — NOT for listing projects.',
      parameters: { type: 'object', properties: { query: { type: 'string', description: 'Symbol name or concept to search for' }, max_tokens: { type: 'integer', description: 'Max tokens to return (default 4000)' } }, required: ['query'] },
    }
  },
  {
    type: 'function',
    function: {
      name: 'explain_symbol',
      description: 'Get full explanation of a symbol: signature, location, callers, callees, and where it fits in the architecture. Use for "what does X do" questions.',
      parameters: { type: 'object', properties: { symbol: { type: 'string', description: 'Symbol name to explain' } }, required: ['symbol'] },
    }
  },
  {
    type: 'function',
    function: {
      name: 'memory_load',
      description: 'Load relevant project memory for a task. Returns past decisions, workflows, conventions, and gotchas. Call this before starting complex work.',
      parameters: { type: 'object', properties: { task: { type: 'string', description: 'Task description to find relevant context for' } }, required: ['task'] },
    }
  },
  {
    type: 'function',
    function: {
      name: 'read_file',
      description: 'Read a file. Returns content with line numbers.',
      parameters: { type: 'object', properties: { path: { type: 'string', description: 'File path relative to cwd' }, start_line: { type: 'integer', description: 'Start line (optional)' }, end_line: { type: 'integer', description: 'End line (optional)' } }, required: ['path'] },
    }
  },
  {
    type: 'function',
    function: {
      name: 'write_file',
      description: 'Create or overwrite a file with content.',
      parameters: { type: 'object', properties: { path: { type: 'string', description: 'File path' }, content: { type: 'string', description: 'Full file content' } }, required: ['path', 'content'] },
    }
  },
  {
    type: 'function',
    function: {
      name: 'patch',
      description: 'Edit a file by replacing old_str with new_str. old_str must match exactly ONE location.',
      parameters: { type: 'object', properties: { path: { type: 'string', description: 'File to edit' }, old_str: { type: 'string', description: 'Exact text to find' }, new_str: { type: 'string', description: 'Replacement text' } }, required: ['path', 'old_str', 'new_str'] },
    }
  },
  {
    type: 'function',
    function: {
      name: 'bash',
      description: 'Run a shell command. Returns stdout/stderr.',
      parameters: { type: 'object', properties: { command: { type: 'string', description: 'Shell command' } }, required: ['command'] },
    }
  },
  {
    type: 'function',
    function: {
      name: 'search',
      description: 'Search file contents using regex (ripgrep). Returns matching lines.',
      parameters: { type: 'object', properties: { pattern: { type: 'string', description: 'Regex pattern' }, path: { type: 'string', description: 'Directory to search (default: .)' } }, required: ['pattern'] },
    }
  },
  {
    type: 'function',
    function: {
      name: 'find_files',
      description: 'Find files matching a glob pattern.',
      parameters: { type: 'object', properties: { pattern: { type: 'string', description: 'Glob pattern e.g. **/*.ts' } }, required: ['pattern'] },
    }
  },
  {
    type: 'function',
    function: {
      name: 'memory_remember',
      description: 'Save durable knowledge to project memory. Only save facts that should persist: decisions, workflows, gotchas, conventions. NOT task transcripts.',
      parameters: { type: 'object', properties: { type: { type: 'string', enum: ['decision', 'workflow', 'gotcha', 'convention', 'context'], description: 'Knowledge type' }, title: { type: 'string', description: 'Short title' }, content: { type: 'string', description: 'The knowledge' }, tags: { type: 'array', items: { type: 'string' }, description: 'Tags' } }, required: ['type', 'title', 'content'] },
    }
  },
  {
    type: 'function',
    function: {
      name: 'bone_compile',
      description: 'Compile a .bone file into a complete Node.js/TypeScript backend. Creates routes, models, auth, events, migrations, SDK, admin panel, Docker, and CI from a single declarative file.',
      parameters: { type: 'object', properties: { path: { type: 'string', description: 'Path to the .bone file' }, target: { type: 'string', description: 'Target: express (default), nakama, prisma, sqlite' } }, required: ['path'] },
    }
  },
  {
    type: 'function',
    function: {
      name: 'bone_check',
      description: 'Validate a .bone file without generating code. Reports type errors and constraint violations. Use this before bone_compile to catch issues early.',
      parameters: { type: 'object', properties: { path: { type: 'string', description: 'Path to the .bone file to validate' } }, required: ['path'] },
    }
  },
];

// Show a compact inline diff for patch operations
function showMiniDiff(filePath, oldStr, newStr, lineNum) {
  const diff = tui.renderDiff(filePath, oldStr, newStr, lineNum);
  if (diff) console.log(diff);
}

// Execute a tool call and return the result
async function executeTool(name, args) {
  const { execSync } = require('child_process');
  const cwd = process.cwd();

  switch (name) {
    case 'read_file': {
      const filePath = path.resolve(cwd, args.path);
      if (!fs.existsSync(filePath)) return { error: `File not found: ${args.path}` };
      const content = fs.readFileSync(filePath, 'utf-8');
      const lines = content.split('\n');
      const start = (args.start_line || 1) - 1;
      const end = args.end_line || lines.length;
      const slice = lines.slice(start, end);
      const numbered = slice.map((l, i) => `${String(start + i + 1).padStart(4)}│ ${l}`).join('\n');
      return { result: `${args.path} (${lines.length} lines):\n${numbered}` };
    }

    case 'write_file': {
      const filePath = path.resolve(cwd, args.path);
      const dir = path.dirname(filePath);
      if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });
      const existed = fs.existsSync(filePath);
      const oldContent = existed ? fs.readFileSync(filePath, 'utf-8') : null;
      fs.writeFileSync(filePath, args.content);
      const lineCount = args.content.split('\n').length;
      const action = existed ? 'Updated' : 'Created';

      // Show diff for overwrites in fullscreen mode
      if (_fullscreenRef && existed && oldContent) {
        const preview = oldContent.split('\n').slice(0, 5).join('\n');
        const newPreview = args.content.split('\n').slice(0, 5).join('\n');
        _fullscreenRef.addDiff(args.path, preview + '\n...', newPreview + '\n...', 1);
      }

      return { result: `${action} ${args.path} (${lineCount} lines)`, action, path: args.path, lines: lineCount };
    }

    case 'patch': {
      const filePath = path.resolve(cwd, args.path);
      if (!fs.existsSync(filePath)) return { error: `File not found: ${args.path}` };
      let content = fs.readFileSync(filePath, 'utf-8');
      const count = content.split(args.old_str).length - 1;
      if (count === 0) return { error: `old_str not found in ${args.path}` };
      if (count > 1) return { error: `old_str matches ${count} locations. Include more context.` };
      content = content.replace(args.old_str, args.new_str);
      fs.writeFileSync(filePath, content);
      const lineNum = content.slice(0, content.indexOf(args.new_str)).split('\n').length;
      const oldLines = args.old_str.split('\n').length;
      const newLines = args.new_str.split('\n').length;

      // Show diff in fullscreen TUI or classic
      if (_fullscreenRef) {
        _fullscreenRef.addDiff(args.path, args.old_str, args.new_str, lineNum);
      } else {
        showMiniDiff(args.path, args.old_str, args.new_str, lineNum);
      }

      return { result: `Patched ${args.path}: replaced ${oldLines} lines with ${newLines} lines at line ${lineNum}`, action: 'Edited', path: args.path, line: lineNum };
    }

    case 'bash': {
      // Auto-translate common Unix commands to Windows (skip on Linux)
      let command = args.command;
      if (process.platform === 'win32') {
        command = command.replace(/^ls\b/, 'dir');
        command = command.replace(/^ls /, 'dir ');
        command = command.replace(/^cat /, 'type ');
        command = command.replace(/^rm -rf /, 'rmdir /s /q ');
        command = command.replace(/^rm /, 'del ');
        command = command.replace(/^touch /, 'echo.>');
        command = command.replace(/^cp /, 'copy ');
        command = command.replace(/^mv /, 'move ');
        command = command.replace(/^mkdir -p /, 'mkdir ');
      }

      // Verbose: show what's being executed
      if (flags.verbose && _fullscreenRef) {
        _fullscreenRef.addTool('bash', 'ok', `$ ${command}`);
      }
      
      try {
        const output = execSync(command, { encoding: 'utf-8', timeout: 30000, cwd, maxBuffer: 1024 * 1024 });
        const maxOutput = (config.context?.detected_window || 32000) < 64000 ? 1500 : 3000;
        const trimmed = output.length > maxOutput ? output.slice(0, maxOutput - 500) + '\n...(truncated)...\n' + output.slice(-300) : output;
        // Verbose: show output
        if (flags.verbose && _fullscreenRef && trimmed.trim()) {
          const lines = trimmed.split('\n').slice(0, 10);
          for (const line of lines) _fullscreenRef.addChat('system', '  ' + line);
        }
        return { result: trimmed || '(no output)', command };
      } catch (e) {
        const output = (e.stdout || '') + (e.stderr || '');
        const exitReason = (e.status === null || e.status === undefined) ? 'Timed out (killed after 30s)' : `Exit code ${e.status}`;
        // Verbose: show error output
        if (flags.verbose && _fullscreenRef && output.trim()) {
          const lines = output.split('\n').slice(0, 8);
          for (const line of lines) _fullscreenRef.addChat('system', '  ' + line);
        }
        return { result: output.slice(0, 2000) || e.message, error: exitReason, command };
      }
    }

    case 'search': {
      try {
        const searchPath = args.path || '.';
        const output = execSync(`rg --line-number --max-count 10 -C 1 "${args.pattern.replace(/"/g, '\\"')}" ${searchPath}`, { encoding: 'utf-8', timeout: 10000, cwd });
        return { result: output.slice(0, 3000) };
      } catch {
        return { result: 'No matches found.' };
      }
    }

    case 'find_files': {
      try {
        const output = execSync(`rg --files --glob "${args.pattern}" --glob "!node_modules" --glob "!.git"`, { encoding: 'utf-8', timeout: 10000, cwd });
        const files = output.trim().split('\n').filter(Boolean).slice(0, 30);
        return { result: files.length ? `Found ${files.length} files:\n${files.join('\n')}` : 'No files found.' };
      } catch {
        return { result: 'No files found.' };
      }
    }

    case 'list_projects': {
      // Get all indexed repos with stats from budget-aware-mcp
      const listResult = await mcpCall('tools/call', { name: 'list_repos', arguments: {} });
      if (listResult && listResult.content) {
        try {
          const data = JSON.parse(listResult.content[0]?.text || '{}');
          const repos = data.repos || [];
          if (repos.length === 0) {
            return { result: 'No projects indexed yet. The code graph is empty.' };
          }
          let output = `Workspace contains ${repos.length} indexed projects:\n\n`;
          for (const r of repos) {
            output += `## ${r.name}\n`;
            output += `  Path: ${r.root_path}\n`;
            output += `  Files: ${r.file_count} | Symbols: ${r.symbol_count} | LOC: ${r.total_loc.toLocaleString()}\n`;
            output += `  Languages: ${(r.languages || []).join(', ') || 'unknown'}\n`;
            output += `  Edges: ${r.edge_count} (connections between symbols)\n`;
            output += `  Last indexed: ${r.last_indexed_at}\n\n`;
          }
          return { result: output };
        } catch (e) {
          return { result: listResult.content[0]?.text || 'Failed to parse repo list.' };
        }
      }
      // Fallback: list directories
      try {
        const entries = fs.readdirSync(process.cwd(), { withFileTypes: true });
        const dirs = entries.filter(e => e.isDirectory() && !e.name.startsWith('.') && e.name !== 'node_modules');
        return { result: `Projects in ${process.cwd()}:\n${dirs.map(d => `  - ${d.name}/`).join('\n')}` };
      } catch {
        return { result: 'Could not list projects.' };
      }
    }

    case 'graph_search': {
      // Delegate to budget-aware-mcp
      const maxTokens = args.max_tokens || 4000;
      const graphResult = await mcpCall('tools/call', { name: 'search_graph', arguments: { query: args.query, max_tokens: maxTokens } });
      if (graphResult && graphResult.content) {
        const text = graphResult.content.map(c => c.text || '').join('\n');
        return { result: text || 'No results from code graph.' };
      }
      // Fallback to ripgrep if MCP not available
      try {
        const output = execSync(`rg --line-number --max-count 5 "${args.query.replace(/"/g, '\\"')}" .`, { encoding: 'utf-8', timeout: 10000, cwd });
        return { result: output.slice(0, 3000) };
      } catch {
        return { result: 'No matches found in code graph or files.' };
      }
    }

    case 'explain_symbol': {
      const graphResult = await mcpCall('tools/call', { name: 'explain_symbol', arguments: { symbol: args.symbol } });
      if (graphResult && graphResult.content) {
        const text = graphResult.content.map(c => c.text || '').join('\n');
        return { result: text || `Symbol "${args.symbol}" not found in code graph.` };
      }
      // Fallback
      try {
        const output = execSync(`rg --line-number "\\b${args.symbol}\\b" . --max-count 10`, { encoding: 'utf-8', timeout: 10000, cwd });
        return { result: `References to ${args.symbol}:\n${output.slice(0, 2000)}` };
      } catch {
        return { result: `Symbol "${args.symbol}" not found.` };
      }
    }

    // ─── Compound Tools (reduce tool call chains) ──────────────────────
    case 'read_and_patch': {
      const filePath = path.resolve(cwd, args.path);
      if (!fs.existsSync(filePath)) return { error: `File not found: ${args.path}` };
      let content = fs.readFileSync(filePath, 'utf-8');
      const count = content.split(args.old_str).length - 1;
      if (count === 0) {
        // Show the file content so model can see what's actually there
        const lines = content.split('\n').slice(0, 50);
        const numbered = lines.map((l, i) => `${(i+1).toString().padStart(4)}| ${l}`).join('\n');
        return { error: `old_str not found. File content:\n${numbered}` };
      }
      if (count > 1) return { error: `old_str matches ${count} locations. Be more specific.` };
      content = content.replace(args.old_str, args.new_str);
      fs.writeFileSync(filePath, content);
      const lineNum = content.slice(0, content.indexOf(args.new_str)).split('\n').length;
      showMiniDiff(args.path, args.old_str, args.new_str, lineNum);
      return { result: `Read and patched ${args.path} at line ${lineNum}`, action: 'Edited', path: args.path, line: lineNum };
    }

    case 'create_and_run': {
      const filePath = path.resolve(cwd, args.path);
      const dir = path.dirname(filePath);
      if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });
      fs.writeFileSync(filePath, args.content);
      const lines = args.content.split('\n').length;
      let output = `Created ${args.path} (${lines} lines)`;
      let cmdError = false;
      if (args.command) {
        try {
          const cmdOut = execSync(args.command, { encoding: 'utf-8', timeout: 30000, cwd, maxBuffer: 1024*1024 });
          output += `\n$ ${args.command}\n${cmdOut.slice(0, 2000)}`;
        } catch (e) {
          cmdError = true;
          const errOut = (e.stdout || '') + (e.stderr || e.message || '');
          output += `\n$ ${args.command}\n${(e.status === null || e.status === undefined) ? 'TIMED OUT' : 'EXIT CODE ' + (e.status || 1)} — FAILED:\n${errOut.slice(0, 2000)}`;
        }
      }
      return { result: output, action: 'Created', path: args.path, lines, error: cmdError ? `Command failed: ${args.command}` : null };
    }

    case 'find_and_read': {
      try {
        const found = execSync(`rg --files --glob "${args.pattern}" --glob "!node_modules" --glob "!.git"`, { encoding: 'utf-8', timeout: 10000, cwd });
        const files = found.trim().split('\n').filter(Boolean);
        if (files.length === 0) return { result: 'No files found matching: ' + args.pattern };
        // Read first match
        const target = files[0];
        const content = fs.readFileSync(path.resolve(cwd, target), 'utf-8');
        const maxLines = args.read_lines || 50;
        const lines = content.split('\n').slice(0, maxLines);
        const numbered = lines.map((l, i) => `${(i+1).toString().padStart(4)}| ${l}`).join('\n');
        let output = `Found ${files.length} files. Reading ${target} (${content.split('\n').length} lines):\n${numbered}`;
        if (files.length > 1) output += `\n\nOther matches: ${files.slice(1, 5).join(', ')}`;
        return { result: output };
      } catch {
        return { result: 'No files found matching: ' + args.pattern };
      }
    }

    case 'search_and_read': {
      try {
        const ctx = args.read_context || 10;
        const output = execSync(`rg --line-number -C ${ctx} --max-count 3 "${args.pattern.replace(/"/g, '\\"')}" . --glob "!node_modules" --glob "!.git"`, { encoding: 'utf-8', timeout: 10000, cwd });
        return { result: output.slice(0, 4000) || 'No matches.' };
      } catch {
        return { result: 'No matches found for: ' + args.pattern };
      }
    }

    case 'run': {
      const timeout = (args.timeout || 30) * 1000;
      try {
        const output = execSync(args.command, { encoding: 'utf-8', timeout, cwd, maxBuffer: 1024*1024 });
        return { result: output.slice(0, 3000) || '(completed with no output)', command: args.command };
      } catch (e) {
        const errOut = (e.stdout || '') + (e.stderr || e.message || '');
        const exitReason = (e.status === null || e.status === undefined) ? 'Timed out (killed after 30s)' : `Exit code ${e.status || 1}`;
        return { result: `${exitReason.toUpperCase()} — FAILED:\n${errOut.slice(0, 2500)}`, error: `Command failed: ${exitReason}`, command: args.command };
      }
    }

    case 'memory_load':
    case 'memory_remember':
    case 'memory_list':
    case 'memory_forget': {
      // Use budget-aware-mcp's SQLite+FTS5 memory store directly
      if (name === 'memory_load') {
        const task = args.task || '';
        const maxTokens = args.max_tokens || 2000;
        const { objects, tokens_used, score_breakdown } = memoryStore.loadForTask(task, maxTokens);
        if (objects.length === 0) return { result: 'No relevant memory found.' };
        const formatted = objects.map(o => `[${o.type}] ${o.title}: ${o.content}`).join('\n\n');
        return { result: `Loaded ${objects.length} memories (${tokens_used} tokens):\n\n${formatted}` };
      }
      if (name === 'memory_remember') {
        const obj = memoryStore.remember({
          type: args.type || 'context',
          title: args.title || '',
          content: args.content || '',
          tags: args.tags || [],
          symbols: args.symbols || [],
          files: args.files || [],
        });
        if (obj.duplicate) return { result: `Already known (confirmed existing: ${obj.existing_id})` };
        return { result: `Remembered [${obj.type}] "${obj.title}" (${obj.id})` };
      }
      if (name === 'memory_list') {
        const objects = args.type ? memoryStore.byType(args.type) : memoryStore.all();
        if (objects.length === 0) return { result: 'No memory stored.' };
        return { result: objects.map(o => `[${o.id}] (${o.type}) ${o.title}`).join('\n') };
      }
      if (name === 'memory_forget') {
        const ok = memoryStore.forget(args.id);
        return { result: ok ? `Deleted ${args.id}` : `Not found: ${args.id}` };
      }
      return { result: '' };
    }

    case 'bone_compile': {
      const bonePath = path.resolve(cwd, args.path);
      if (!fs.existsSync(bonePath)) return { error: `File not found: ${args.path}` };
      if (!args.path.endsWith('.bone')) return { error: `Expected a .bone file, got: ${args.path}` };
      const target = args.target || 'express';
      // Find the BoneScript compiler
      const compilerPaths = [
        path.resolve(__dirname, '..', 'node_modules', 'bonescript-compiler', 'dist', 'cli.js'),
        path.resolve(__dirname, '..', '..', 'BoneScript', 'compiler', 'dist', 'cli.js'),
      ];
      let compiler = null;
      for (const cp of compilerPaths) {
        if (fs.existsSync(cp)) { compiler = cp; break; }
      }
      if (!compiler) return { error: 'BoneScript compiler not found. Run: npm install (ensure bonescript-compiler dependency is linked)' };
      try {
        const output = execSync(`node "${compiler}" compile "${bonePath}" --target ${target}`, { encoding: 'utf-8', timeout: 30000, cwd });
        return { result: `Compiled ${args.path} → output/\n${output.slice(0, 2000)}`, action: 'Created', path: 'output/' };
      } catch (e) {
        const errOut = (e.stdout || '') + (e.stderr || e.message || '');
        return { error: `BoneScript compile failed:\n${errOut.slice(0, 2000)}` };
      }
    }

    case 'bone_check': {
      const bonePath = path.resolve(cwd, args.path);
      if (!fs.existsSync(bonePath)) return { error: `File not found: ${args.path}` };
      if (!args.path.endsWith('.bone')) return { error: `Expected a .bone file, got: ${args.path}` };
      const compilerPaths = [
        path.resolve(__dirname, '..', 'node_modules', 'bonescript-compiler', 'dist', 'cli.js'),
        path.resolve(__dirname, '..', '..', 'BoneScript', 'compiler', 'dist', 'cli.js'),
      ];
      let compiler = null;
      for (const cp of compilerPaths) {
        if (fs.existsSync(cp)) { compiler = cp; break; }
      }
      if (!compiler) return { error: 'BoneScript compiler not found. Run: npm install (ensure bonescript-compiler dependency is linked)' };
      try {
        const output = execSync(`node "${compiler}" check "${bonePath}"`, { encoding: 'utf-8', timeout: 15000, cwd });
        return { result: output.trim() || '✓ No errors found.' };
      } catch (e) {
        const errOut = (e.stdout || '') + (e.stderr || e.message || '');
        return { error: `BoneScript validation errors:\n${errOut.slice(0, 2000)}` };
      }
    }

    case 'web_search': {
      if (process.env.SMALLCODE_WEB_BROWSE !== 'true') {
        return { error: 'Web browsing is disabled. Set SMALLCODE_WEB_BROWSE=true in .env to enable (recommended for 20B+ models).' };
      }
      const { webSearch } = require('../src/tools/builtin/web_browse');
      const results = await webSearch(args.query, 5);
      const formatted = results.map((r, i) => `${i + 1}. ${r.title}\n   ${r.url}\n   ${r.snippet}`).join('\n\n');
      return { result: formatted || 'No results found.' };
    }

    case 'web_fetch': {
      if (process.env.SMALLCODE_WEB_BROWSE !== 'true') {
        return { error: 'Web browsing is disabled. Set SMALLCODE_WEB_BROWSE=true in .env to enable.' };
      }
      const { webFetch } = require('../src/tools/builtin/web_browse');
      const content = await webFetch(args.url, 5000);
      return { result: content || 'Failed to fetch URL.' };
    }

    default: {
      // Try plugin tools before giving up
      if (pluginLoader) {
        const pluginResult = await pluginLoader.executeTool(name, args);
        if (pluginResult !== null) {
          if (pluginResult.error) return { error: pluginResult.error };
          const output = typeof pluginResult === 'string' ? pluginResult : JSON.stringify(pluginResult);
          return { result: output };
        }
      }
      return { error: `Unknown tool: ${name}` };
    }
  }
}

// ─── COMPOUND TOOLS ──────────────────────────────────────────────────────────
// These do multiple operations in one tool call so the model doesn't need to
// chain 3-4 calls together. Critical for small models that lose coherence
// after too many sequential tool calls.

const COMPOUND_TOOLS = [
  {
    type: 'function',
    function: {
      name: 'read_and_patch',
      description: 'Read a file, then apply a patch to it in one step. Equivalent to read_file + patch but in a single tool call.',
      parameters: { type: 'object', properties: { path: { type: 'string', description: 'File path' }, old_str: { type: 'string', description: 'Text to find' }, new_str: { type: 'string', description: 'Replacement text' } }, required: ['path', 'old_str', 'new_str'] },
    }
  },
  {
    type: 'function',
    function: {
      name: 'create_and_run',
      description: 'Create a file and then run a command (like running the file or running tests). Equivalent to write_file + bash in one call.',
      parameters: { type: 'object', properties: { path: { type: 'string', description: 'File to create' }, content: { type: 'string', description: 'File content' }, command: { type: 'string', description: 'Command to run after creating' } }, required: ['path', 'content'] },
    }
  },
  {
    type: 'function',
    function: {
      name: 'find_and_read',
      description: 'Find files matching a pattern and read the first match. Equivalent to find_files + read_file in one call.',
      parameters: { type: 'object', properties: { pattern: { type: 'string', description: 'Glob pattern (e.g. **/main.ts, src/**/*.py)' }, read_lines: { type: 'integer', description: 'Max lines to show from matched file. Default: 50' } }, required: ['pattern'] },
    }
  },
  {
    type: 'function',
    function: {
      name: 'search_and_read',
      description: 'Search for a pattern in code, then read the most relevant file found. Equivalent to search + read_file in one call.',
      parameters: { type: 'object', properties: { pattern: { type: 'string', description: 'Regex to search for' }, read_context: { type: 'integer', description: 'Lines of context around matches. Default: 10' } }, required: ['pattern'] },
    }
  },
  {
    type: 'function',
    function: {
      name: 'run',
      description: 'Run an existing file (python, node, etc). Use this instead of create_and_run when the file already exists.',
      parameters: { type: 'object', properties: { command: { type: 'string', description: 'Command to run e.g. "python game.py" or "node server.js"' }, timeout: { type: 'integer', description: 'Timeout in seconds. Default: 30' } }, required: ['command'] },
    }
  },
];

// Merge compound tools with base tools (plugins added at runtime)
function getAllTools() {
  const pluginTools = pluginLoader ? pluginLoader.getTools() : [];
  return [...TOOLS, ...COMPOUND_TOOLS, ...pluginTools];
}
let ALL_TOOLS = [...TOOLS, ...COMPOUND_TOOLS];

const MAX_TOOL_CALLS = 500;
const MAX_IMPROVE_ITERATIONS = 2;

async function runAgentLoop(userMessage, config) {
  // Clarification loop — detect vague prompts before wasting tool calls
  const { needsClarification, getClarificationInstruction } = require('../src/session/clarify');
  if (needsClarification(userMessage)) {
    // Inject clarification instruction into this turn only
    conversationHistory.push({ role: 'user', content: userMessage });
    conversationHistory.push({ role: 'system', content: getClarificationInstruction() });
    // Let the model ask for clarification (no tools, just respond)
    const response = await chatCompletion(config, conversationHistory);
    const message = response?.choices?.[0]?.message;
    if (message?.content) {
      conversationHistory.push({ role: 'assistant', content: message.content });
      if (_fullscreenRef) {
        _fullscreenRef.addChat('assistant', message.content);
      } else {
        process.stdout.write(tui.renderMarkdown(message.content));
      }
    }
    return; // Wait for user to clarify
  }

  // Detect drag-and-dropped image files (bare path pasted into terminal)
  const { detectDroppedFile } = require('../src/session/images');
  const droppedPath = detectDroppedFile(userMessage);
  if (droppedPath) {
    // Convert bare path into an @reference prompt
    userMessage = `@${droppedPath} — I dropped this image. What would you like me to do with it?`;
    if (_fullscreenRef) _fullscreenRef.addTool('image', 'ok', `attached: ${path.basename(droppedPath)}`);
  }

  // Resolve @file references in user input
  const { text, files } = resolveReferences(userMessage, process.cwd());
  let augmented = files.length > 0
    ? text + formatReferencesForPrompt(files)
    : text;

  // Auto-inject git diff when message implies recent changes
  if (shouldInjectGitContext(userMessage)) {
    const gitCtx = getGitDiffContext(process.cwd(), 80);
    if (gitCtx) augmented += gitCtx;
  }

  conversationHistory.push({ role: 'user', content: augmented });

  // Governor: classify task type (determines verification strategy)
  currentTaskType = classifyTask(userMessage);

  // Multi-model routing: pick model based on task complexity (if configured)
  if (config.models) {
    const selectedModel = routeModel(userMessage, config);
    if (selectedModel !== config.model.name) {
      config.model.name = selectedModel;
      if (_fullscreenRef) _fullscreenRef.addTool('router', 'ok', `→ ${selectedModel}`);
    }
  }

  // Auto-compact: estimate tokens and aggressively trim to stay within context window
  const estimatedTokens = conversationHistory.reduce((sum, m) => {
    const content = typeof m.content === 'string' ? m.content : JSON.stringify(m.content || '');
    return sum + Math.ceil(content.length / 4);
  }, 0);
  const maxContextTokens = (config.context?.detected_window || 32000) * ((config.context?.max_budget_pct || 70) / 100);

  if (estimatedTokens > maxContextTokens || conversationHistory.length > 30) {
    // Trim oldest messages but preserve system/skill injections
    while (conversationHistory.length > 6) {
      const currentEst = conversationHistory.reduce((sum, m) => {
        const c = typeof m.content === 'string' ? m.content : JSON.stringify(m.content || '');
        return sum + Math.ceil(c.length / 4);
      }, 0);
      if (currentEst < maxContextTokens * 0.7 && conversationHistory.length <= 20) break;
      // Find first non-system message to remove (preserve skills/plugins)
      const removeIdx = conversationHistory.findIndex(m => m.role !== 'system');
      if (removeIdx === -1) break; // All system messages — can't trim further
      conversationHistory.splice(removeIdx, 1);
    }
    const summary = `[Context compacted to fit ${Math.round(maxContextTokens)} token budget]`;
    conversationHistory.unshift({ role: 'system', content: summary });
    console.log(tui.compacted(conversationHistory.length));
  }

  let toolCallsThisTurn = 0;

  while (toolCallsThisTurn < MAX_TOOL_CALLS) {
    const response = await chatCompletion(config, conversationHistory);

    if (!response) {
      console.log('  \x1b[31m✗ No response from model\x1b[0m');
      break;
    }

    const message = response.choices?.[0]?.message;
    if (!message) break;

    // If model wants to call tools
    if (message.tool_calls && message.tool_calls.length > 0) {
      // Add assistant message with tool calls to history
      conversationHistory.push(message);

      for (const tc of message.tool_calls) {
        toolCallsThisTurn++;
        const toolName = tc.function.name;
        let toolArgs;
        try {
          toolArgs = JSON.parse(tc.function.arguments);
        } catch {
          toolArgs = {};
          console.log(`  \x1b[31m✗ Failed to parse args for ${toolName}\x1b[0m`);
        }

        // Show what's happening
        process.stdout.write(tui.toolStart(toolName));
        const toolStart2 = Date.now();

        const result = await executeTool(toolName, toolArgs);
        const toolMs = Date.now() - toolStart2;

        // Show result indicators
        if (result.error) {
          console.log(tui.toolError(result.error));
        } else if (result.action === 'Created') {
          console.log(tui.toolCreated(result.path, result.lines, toolMs));
        } else if (result.action === 'Updated') {
          console.log(tui.toolUpdated(result.path, result.lines, toolMs));
        } else if (result.action === 'Edited') {
          console.log(tui.toolEdited(result.path, result.line, toolMs));
        } else if (result.command) {
          console.log(tui.toolBash(result.command, toolMs));
        } else {
          console.log(tui.toolSuccess('', toolMs));
        }

        // Add tool result to history
        conversationHistory.push({
          role: 'tool',
          tool_call_id: tc.id,
          content: result.result || result.error || '',
        });

        // ── IMPROVEMENT LOOP: auto-validate writes and feed errors back ──
        if ((toolName === 'write_file' || toolName === 'patch') && !result.error) {
          const filePath = toolArgs.path;
          const validation = runValidation(filePath);
          if (validation && !validation.passed) {
            // Track how many times we've tried fixing this file
            if (!improvementAttempts[filePath]) improvementAttempts[filePath] = 0;
            improvementAttempts[filePath]++;

            if (improvementAttempts[filePath] <= MAX_IMPROVE_ITERATIONS) {
              const attempt = improvementAttempts[filePath];
              console.log(tui.improvementLoop(validation.errors, attempt, MAX_IMPROVE_ITERATIONS));

              // Track attempt history for this file
              if (!improvementAttempts[`__history:${filePath}`]) improvementAttempts[`__history:${filePath}`] = [];
              improvementAttempts[`__history:${filePath}`].push({
                attempt,
                errors: validation.errors.slice(0, 3),
              });

              // Build fix prompt with full retry history
              let fixPrompt;
              const history = improvementAttempts[`__history:${filePath}`];
              const historyStr = history.length > 1
                ? `\n\nPrevious attempts (${history.length - 1} failed):\n` + history.slice(0, -1).map((h, i) => `  Attempt ${i + 1}: ${h.errors[0] || 'unknown error'}`).join('\n')
                : '';

              if (attempt <= 2) {
                fixPrompt = `[AUTO-VALIDATE] Errors in ${filePath} (attempt ${attempt}/${MAX_IMPROVE_ITERATIONS}):
${validation.errors.join('\n')}${historyStr}

Fix these errors. Do NOT repeat the same approach that failed before.`;
              } else {
                // Escalated: show the full file + errors + history
                let fileContent = '';
                try { fileContent = fs.readFileSync(path.resolve(process.cwd(), filePath), 'utf-8'); } catch {}
                fixPrompt = `[AUTO-VALIDATE] After ${attempt} attempts, ${filePath} still has errors.${historyStr}

FULL FILE CONTENT:
\`\`\`
${fileContent}
\`\`\`

ERRORS:
${validation.errors.join('\n')}

Read the FULL file above carefully. Fix ALL errors. Use the patch tool with the exact text from the file. Do NOT repeat previous failed approaches.`;
              }

              conversationHistory.push({ role: 'user', content: fixPrompt });
            } else {
              // DECOMPOSE instead of giving up — break the problem into chunks
              improvementAttempts[filePath] = 0;
              const { pickDecomposeStrategy } = require('./governor');
              let fileContent = '';
              try { fileContent = fs.readFileSync(path.resolve(process.cwd(), filePath), 'utf-8'); } catch {}
              const strategy = pickDecomposeStrategy(fileContent, validation.errors, filePath);
              
              // Track decompose attempts — if this is the 2nd decompose, escalate instead
              if (!improvementAttempts[`__decompose:${filePath}`]) improvementAttempts[`__decompose:${filePath}`] = 0;
              improvementAttempts[`__decompose:${filePath}`]++;

              if (improvementAttempts[`__decompose:${filePath}`] >= 2 && escalationEngine && escalationEngine.canEscalate()) {
                // Decompose has been tried and failed — ESCALATE to stronger model
                console.log(`  \x1b[35m⬆ ESCALATING to ${escalationEngine.provider} (${escalationEngine.model}) — local model exhausted\x1b[0m`);
                
                const escalationPrompt = `Fix these errors in ${filePath}. The code:\n\`\`\`\n${fileContent}\n\`\`\`\n\nErrors:\n${validation.errors.join('\n')}\n\nPrevious attempts failed. Fix it correctly.`;
                const escalationMessages = [
                  ...conversationHistory.slice(-6), // Recent context
                  { role: 'user', content: escalationPrompt },
                ];
                
                const escalatedResponse = await escalationEngine.escalate(escalationMessages, ALL_TOOLS);
                
                if (escalatedResponse && !escalatedResponse.error) {
                  // Inject the escalated response back into the conversation
                  if (escalatedResponse.tool_calls) {
                    conversationHistory.push(escalatedResponse);
                    // Execute the escalated model's tool calls
                    for (const tc of escalatedResponse.tool_calls) {
                      const eName = tc.function.name;
                      let eArgs;
                      try { eArgs = JSON.parse(tc.function.arguments); } catch { eArgs = {}; }
                      process.stdout.write(`  \x1b[35m⬆\x1b[0m `);
                      process.stdout.write(tui.toolStart(eName));
                      const eResult = await executeTool(eName, eArgs);
                      if (eResult.error) {
                        console.log(tui.toolError(eResult.error));
                      } else if (eResult.action) {
                        console.log(tui.toolSuccess(`${eResult.action} ${eResult.path || ''}`, 0));
                      } else {
                        console.log(tui.toolSuccess('', 0));
                      }
                      conversationHistory.push({ role: 'tool', tool_call_id: tc.id, content: eResult.result || eResult.error || '' });
                    }
                  } else if (escalatedResponse.content) {
                    conversationHistory.push({ role: 'assistant', content: escalatedResponse.content });
                    process.stdout.write(tui.renderMarkdown(escalatedResponse.content));
                  }
                  improvementAttempts[`__decompose:${filePath}`] = 0;
                } else {
                  // Escalation also failed — give up gracefully
                  const errMsg = escalatedResponse?.error || 'No response';
                  console.log(`  \x1b[31m✗ Escalation failed: ${errMsg}\x1b[0m`);
                  conversationHistory.push({
                    role: 'user',
                    content: `[ESCALATION FAILED] Even the stronger model couldn't fix this. Deliver the best version you have and explain what's still broken.`,
                  });
                }
              } else {
                // First decompose attempt — try the local model with a new strategy
                console.log(`  \x1b[33m◇ DECOMPOSE: ${strategy.reason}\x1b[0m`);
                console.log(`  \x1b[90m  Strategy: ${strategy.type}\x1b[0m`);
                
                conversationHistory.push({
                  role: 'user',
                  content: `[DECOMPOSE] After ${MAX_IMPROVE_ITERATIONS} failed fix attempts, changing strategy.\n\n${strategy.instruction}`,
                });
              }
            }
          } else if (validation && validation.passed) {
            if (improvementAttempts[filePath] > 0) {
              console.log(tui.improvementFixed(filePath, improvementAttempts[filePath]));
              improvementAttempts[filePath] = 0;
            }
          }
        }

        // ── IMPROVEMENT LOOP: auto-validate bash/run commands that fail ──
        if ((toolName === 'bash' || toolName === 'run' || toolName === 'create_and_run') && result.error) {
          if (!improvementAttempts['__bash']) improvementAttempts['__bash'] = 0;
          improvementAttempts['__bash']++;

          if (improvementAttempts['__bash'] <= 2) {
            conversationHistory.push({
              role: 'user',
              content: `[AUTO-FIX] The command FAILED (attempt ${improvementAttempts['__bash']}/2). Do NOT claim success. The error was:\n${(result.result || '').slice(0, 1500)}\n\nRead the error, identify the bug, and fix it.`,
            });
          } else {
            // DECOMPOSE: bash keeps failing, break the problem apart
            if (!improvementAttempts['__decompose:bash']) improvementAttempts['__decompose:bash'] = 0;
            improvementAttempts['__decompose:bash']++;

            if (improvementAttempts['__decompose:bash'] >= 2 && escalationEngine && escalationEngine.canEscalate()) {
              // Bash decompose failed twice — escalate
              console.log(`  \x1b[35m⬆ ESCALATING to ${escalationEngine.provider} (${escalationEngine.model}) — command keeps failing\x1b[0m`);
              improvementAttempts['__bash'] = 0;
              improvementAttempts['__decompose:bash'] = 0;

              const escalationMessages = [
                ...conversationHistory.slice(-8),
                { role: 'user', content: `The command keeps failing. Fix the underlying issue. Error: ${(result.result || '').slice(0, 1500)}` },
              ];
              
              const escalatedResponse = await escalationEngine.escalate(escalationMessages, ALL_TOOLS);
              if (escalatedResponse && !escalatedResponse.error) {
                if (escalatedResponse.tool_calls) {
                  conversationHistory.push(escalatedResponse);
                  for (const tc of escalatedResponse.tool_calls) {
                    const eName = tc.function.name;
                    let eArgs;
                    try { eArgs = JSON.parse(tc.function.arguments); } catch { eArgs = {}; }
                    process.stdout.write(`  \x1b[35m⬆\x1b[0m `);
                    process.stdout.write(tui.toolStart(eName));
                    const eResult = await executeTool(eName, eArgs);
                    if (eResult.error) {
                      console.log(tui.toolError(eResult.error));
                    } else {
                      console.log(tui.toolSuccess('', 0));
                    }
                    conversationHistory.push({ role: 'tool', tool_call_id: tc.id, content: eResult.result || eResult.error || '' });
                  }
                } else if (escalatedResponse.content) {
                  conversationHistory.push({ role: 'assistant', content: escalatedResponse.content });
                  process.stdout.write(tui.renderMarkdown(escalatedResponse.content));
                }
              } else {
                console.log(`  \x1b[31m✗ Escalation failed\x1b[0m`);
                conversationHistory.push({
                  role: 'user',
                  content: `[ESCALATION FAILED] Move on. Explain what you tried and what's still broken.`,
                });
              }
            } else {
              // First bash decompose — try local model with new strategy
              improvementAttempts['__bash'] = 0;
              const { pickDecomposeStrategy } = require('./governor');
              const errors = [(result.result || '').slice(0, 300)];
              const strategy = pickDecomposeStrategy('', errors, toolArgs.command || '');
              console.log(`  \x1b[33m◇ DECOMPOSE: Command keeps failing. Changing approach.\x1b[0m`);
              conversationHistory.push({
                role: 'user',
                content: `[DECOMPOSE] The command has failed 3 times. STOP retrying the same approach.\n\n${strategy.instruction}`,
              });
            }
          }
        } else if ((toolName === 'bash' || toolName === 'run') && !result.error) {
          improvementAttempts['__bash'] = 0;
        }

        // ── GOVERNOR: Record tool success/failure for Bayesian learning ──
        if (!result.error) {
          toolScorer.recordSuccess(toolName, currentTaskType, toolMs);
        } else {
          toolScorer.recordFailure(toolName, currentTaskType, result.error || 'unknown');
        }

        // ── PLUGINS: Fire post_tool hooks ──
        if (pluginLoader && pluginLoader.hooks.length > 0) {
          for (const hook of pluginLoader.hooks) {
            if (hook.event === 'post_tool' && hook.handler) {
              if (hook.filter.length === 0 || hook.filter.includes(toolName)) {
                try { await hook.handler({ tool: toolName, args: toolArgs, result, ms: toolMs }); } catch {}
              }
            }
          }
        }
      }

      // Continue the loop — model may want to call more tools or fix errors
      continue;
    }

    // No tool calls — model is responding with text
    // Counter guard: if this is a coding/editing task and no tools were called,
    // the model may be prematurely answering instead of acting
    if (toolCallsThisTurn === 0 && (currentTaskType === 'coding' || currentTaskType === 'editing' || currentTaskType === 'backend')) {
      if (message.content && !message.content.includes('?') && message.content.length < 200) {
        // Model gave a short non-question response without using tools — push it to act
        conversationHistory.push({ role: 'assistant', content: message.content });
        conversationHistory.push({ role: 'user', content: '[SYSTEM] You responded without using any tools. This task requires file operations. Please use the appropriate tools (read_file, write_file, patch, etc.) to complete the task. Do not just describe what you would do — actually do it.' });
        continue;
      }
    }

    // Greeting guard: detect when model outputs a greeting after tool failures (lost context)
    if (toolCallsThisTurn > 0 && message.content) {
      const lc = message.content.toLowerCase();
      const isGreeting = lc.includes('how can i help') || lc.includes('what would you like') || lc.includes('what can i') || (lc.startsWith('hey') && lc.includes('assist'));
      if (isGreeting && conversationHistory.some(m => m.role === 'user' && !m.content.startsWith('['))) {
        conversationHistory.push({ role: 'assistant', content: message.content });
        conversationHistory.push({ role: 'user', content: '[SYSTEM] You output a greeting instead of completing the task. Look at the conversation above — there is still work to do. If a tool failed, explain what went wrong. Do NOT restart the conversation.' });
        continue;
      }
    }

    // Post-decompose give-up detection: if model responds with vague text after failures, notify user
    if (toolCallsThisTurn > 0 && message.content) {
      const lc = message.content.toLowerCase();
      const gaveUp = lc.includes('output is truncated') || lc.includes('let me try') || lc.includes('let me run') || (lc.length < 100 && !lc.includes('?') && toolCallsThisTurn > 3);
      const hadDecompose = conversationHistory.some(m => m.content && m.content.includes('[DECOMPOSE]'));
      if (gaveUp && hadDecompose && escalationEngine && escalationEngine.canEscalate()) {
        // Model is stuck after decompose — offer escalation
        if (_fullscreenRef) {
          _fullscreenRef.addTool('escalation', 'err', 'Model stuck after decompose. Attempting escalation...');
        }
        conversationHistory.push({ role: 'assistant', content: message.content });
        conversationHistory.push({ role: 'user', content: '[SYSTEM] You appear stuck. The decompose strategy did not work. Take a completely different approach or clearly explain what is blocking you and what you need from the user to proceed.' });
        continue;
      }
    }

    // Stream the final response for better UX
    if (message.content) {
      conversationHistory.push({ role: 'assistant', content: message.content });
      // Render with markdown highlighting
      process.stdout.write(tui.renderMarkdown(message.content));
    } else if (!message.tool_calls || message.tool_calls.length === 0) {
      // No content AND no tool calls — try streaming for the response
      const streamedContent = await streamFinalResponse(config, conversationHistory);
      if (streamedContent) {
        conversationHistory.push({ role: 'assistant', content: streamedContent });
      }
    }
    break;
  }

  if (toolCallsThisTurn >= MAX_TOOL_CALLS) {
    console.log(chalk.yellow('\n  ⚠ Reached tool call limit'));
  }

  if (toolCallsThisTurn > 0) {
    console.log(tui.turnSummary(toolCallsThisTurn));

    // Auto git commit if files were changed and we're in a git repo
    if (config.git?.auto_commit === true || process.env.SMALLCODE_AUTO_COMMIT === 'true') {
      try {
        const { execSync } = require('child_process');
        const status = execSync('git status --porcelain', { encoding: 'utf-8', cwd: process.cwd(), timeout: 5000 });
        if (status.trim()) {
          // Get a short summary from the first user message this turn
          const lastUser = [...conversationHistory].reverse().find(m => m.role === 'user' && !m.content.startsWith('['));
          const commitMsg = lastUser
            ? `smallcode: ${lastUser.content.slice(0, 50).replace(/\n/g, ' ')}`
            : 'smallcode: auto-commit';
          execSync('git add -A', { cwd: process.cwd(), timeout: 5000 });
          execSync(`git commit -m "${commitMsg.replace(/"/g, '\\"')}"`, { encoding: 'utf-8', cwd: process.cwd(), timeout: 10000 });
          if (_fullscreenRef) {
            _fullscreenRef.addTool('git', 'ok', `committed: ${commitMsg.slice(0, 60)}`);
          } else {
            console.log(chalk.green(`  ✓ git commit: ${commitMsg.slice(0, 60)}`));
          }
        }
      } catch {
        // Not a git repo or commit failed — silently skip
      }
    }
  }
}

// ─── Validation for Improvement Loop ────────────────────────────────────────

// LSP client instance (lazy-initialized on first validation)
let _lspClient = null;
let _lspAttempted = false;

async function initLSP() {
  if (_lspAttempted) return _lspClient;
  _lspAttempted = true;
  try {
    const { LSPClient } = require('../src/lsp/client');
    const client = new LSPClient(process.cwd());
    const ok = await client.start();
    if (ok) {
      _lspClient = client;
      if (_fullscreenRef) _fullscreenRef.addTool('lsp', 'ok', `${client.serverInfo.language} language server connected`);
    }
  } catch {}
  return _lspClient;
}

function runValidation(filePath) {
  const { execSync } = require('child_process');
  const ext = path.extname(filePath);
  const cwd = process.cwd();

  let cmd = null;
  let parseErrors = null;

  // TypeScript
  if ((ext === '.ts' || ext === '.tsx') && fs.existsSync(path.join(cwd, 'tsconfig.json'))) {
    cmd = 'npx tsc --noEmit --pretty false 2>&1';
    parseErrors = (output) => {
      return output.split('\n')
        .filter(l => l.includes(filePath) && l.includes('error'))
        .slice(0, 5);
    };
  }
  // Python
  else if (ext === '.py') {
    cmd = `python -m py_compile "${filePath}" 2>&1`;
    parseErrors = (output) => output.trim() ? [output.trim()] : [];
  }
  // Rust
  else if (ext === '.rs' && fs.existsSync(path.join(cwd, 'Cargo.toml'))) {
    cmd = 'cargo check --message-format short 2>&1';
    parseErrors = (output) => {
      return output.split('\n')
        .filter(l => l.startsWith('error'))
        .slice(0, 5);
    };
  }
  // Go
  else if (ext === '.go' && fs.existsSync(path.join(cwd, 'go.mod'))) {
    cmd = 'go build ./... 2>&1';
    parseErrors = (output) => {
      return output.split('\n')
        .filter(l => l.includes(filePath))
        .slice(0, 5);
    };
  }
  // JavaScript/Node (eslint or basic syntax check)
  else if (ext === '.js' || ext === '.mjs') {
    cmd = `node --check "${filePath}" 2>&1`;
    parseErrors = (output) => output.trim() ? [output.trim()] : [];
  }
  // JSON validation
  else if (ext === '.json') {
    try {
      JSON.parse(fs.readFileSync(path.resolve(process.cwd(), filePath), 'utf-8'));
      return { passed: true, errors: [] };
    } catch (e) {
      return { passed: false, errors: [e.message] };
    }
  }
  // BoneScript validation
  else if (ext === '.bone') {
    const compilerPaths = [
      path.resolve(__dirname, '..', 'node_modules', 'bonescript-compiler', 'dist', 'cli.js'),
      path.resolve(__dirname, '..', '..', 'BoneScript', 'compiler', 'dist', 'cli.js'),
    ];
    let compiler = null;
    for (const cp of compilerPaths) {
      if (fs.existsSync(cp)) { compiler = cp; break; }
    }
    if (!compiler) return null; // No validator available
    // Try bone_check — if it crashes (not installed properly), skip validation
    try {
      const { execSync } = require('child_process');
      execSync(`node "${compiler}" --version`, { encoding: 'utf-8', timeout: 5000, cwd: process.cwd() });
    } catch {
      return null; // Compiler not working, skip .bone validation
    }
    cmd = `node "${compiler}" check "${filePath}" 2>&1`;
    parseErrors = (output) => output.split('\n').filter(l => l.includes('error')).slice(0, 5);
  }

  if (!cmd) return null;  // No validator for this file type

  try {
    execSync(cmd, { encoding: 'utf-8', timeout: 20000, cwd });
    return { passed: true, errors: [] };
  } catch (e) {
    const output = (e.stdout || '') + (e.stderr || '');
    const errors = parseErrors(output).filter(Boolean);
    if (errors.length === 0) return { passed: true, errors: [] };
    return { passed: false, errors };
  }
}

// Auto-load relevant memory for the current task (injected into system prompt)
function getMemoryContext(messages) {
  try {
    // Get the last user message to find relevant memory
    const lastUser = [...messages].reverse().find(m => m.role === 'user');
    if (!lastUser || !memoryStore.loadForTask) return '';

    const { objects, tokens_used } = memoryStore.loadForTask(lastUser.content, 800);
    if (objects.length === 0) return '';

    return '\n\nRelevant project memory:\n' + objects.map(o => `[${o.type}] ${o.title}: ${o.content}`).join('\n');
  } catch {
    return '';
  }
}

// Auto-load relevant skills based on the user's message
function getSkillContext(messages) {
  if (!skillManager) return '';
  try {
    const lastUser = [...messages].reverse().find(m => m.role === 'user');
    if (!lastUser) return '';
    const skills = skillManager.getAutoSkills(lastUser.content);
    return skillManager.formatForPrompt(skills);
  } catch {
    return '';
  }
}

// Get plugin prompt injections for the current task type
function getPluginPrompts() {
  if (!pluginLoader) return '';
  try {
    const injection = pluginLoader.getPromptInjections(currentTaskType);
    return injection ? '\n\n' + injection : '';
  } catch {
    return '';
  }
}

// Make a chat completion request (non-streaming for tool use, streaming for final response)
async function chatCompletion(config, messages) {
  const baseUrl = config.model.baseUrl;
  const systemMsg = {
    role: 'system',
    content: `You are SmallCode, a coding assistant that operates in the user's project directory.

You have tools to read, write, and edit files, run shell commands, and search code.
You also have project memory and compound tools that do multiple operations in one call.
You have a CODE GRAPH indexed for this project — use it for understanding questions.

IMPORTANT — Code Graph (use these FIRST for understanding/analysis questions):
- list_projects: Lists ALL projects in the workspace with stats (files, symbols, LOC, languages). Use FIRST when asked "what projects are here" or "tell me about the codebase".
- graph_search: Search for a specific symbol/function/class in the graph. Use for "how does X work" or "find the auth logic". NOT for listing projects.
- explain_symbol: Get full explanation of a function/class — signature, location, callers, callees, and architecture context.
- memory_load: Load relevant project memory (past decisions, conventions, gotchas).

When the user asks about the codebase, architecture, or "how does X work":
1. For "what projects" / "describe the workspace" → use list_projects
2. For "how does function X work" → use graph_search or explain_symbol
3. THEN read specific files only if you need more detail
4. Do NOT manually read every file — the graph has the relationships already indexed
5. For multi-project workspaces: the graph indexes ALL sub-projects. Search broadly.

IMPORTANT — Environment:
- OS: Windows (cmd.exe shell)
- Use "dir" not "ls", "type" not "cat", "del" not "rm", "mkdir" not "mkdir -p"
- Use backslashes in paths or forward slashes (both work)
- Do NOT use bash-specific commands (touch, export, chmod)

Rules:
- PREFER compound tools to reduce back-and-forth: use read_and_patch instead of read_file then patch, use find_and_read instead of find_files then read_file, use create_and_run instead of write_file then bash.
- Use the "patch" or "read_and_patch" tool for edits. Do NOT rewrite whole files.
- Be concise — show what you did, not lengthy explanations.
- If a task has multiple steps, use compound tools to minimize calls.
- When you're done, say what you accomplished in 1-2 sentences.
- If a tool fails, explain what went wrong and what you tried. Do NOT ignore errors and output a generic greeting.
- NEVER output a greeting or "how can I help" message in the middle of a task. Stay focused on the current work.
- Create files with write_file directly. Do NOT run mkdir first — write_file creates directories automatically.
- Do NOT delete and recreate files to fix errors. Use the patch tool to fix the specific broken lines.
${currentTaskType === 'backend' ? `
BONESCRIPT MODE — For Node.js/TypeScript backends, use BoneScript:
1. Write a .bone file (declarative syntax — entities, capabilities, events, policies)
2. Validate with bone_check tool
3. Compile with bone_compile tool → generates ENTIRE project (routes, auth, DB, events, migrations, SDK, admin panel)

Do NOT hand-write Express routes, Prisma schemas, or JWT auth. Use BoneScript instead.

BoneScript syntax:
system MyApp {
  entity User {
    owns: [name: string, email: string, role: string]
    constraints: [email.unique, name.length in 1..100]
    states: active -> suspended -> deleted
    auth: jwt
  }
  capability register(user: User) {
    requires: [user.email.unique]
    effects: [user.state = active]
    emits: UserRegistered
    sync: transactional
  }
  event UserRegistered {
    payload: { user_id: uuid, email: string }
    delivery: exactly_once
  }
  policy api { rate_limit: 100 per 1m, audit: true }
}
` : ''}
Working directory: ${process.cwd()}
${getMemoryContext(messages)}${getSkillContext(messages)}${getPluginPrompts()}`
  };

  try {
    // Transform messages with images into multimodal format
    const { extractImages, formatImagesForAPI, modelSupportsVision } = require('../src/session/images');
    const processedMessages = messages.map(msg => {
      if (msg.role !== 'user' || typeof msg.content !== 'string') return msg;
      const images = extractImages(msg.content, process.cwd());
      if (images.length === 0 || !modelSupportsVision(config.model.name)) return msg;
      // Convert to multimodal content array
      return {
        ...msg,
        content: [
          { type: 'text', text: msg.content },
          ...formatImagesForAPI(images),
        ],
      };
    });

    const body = {
      model: config.model.name,
      messages: [systemMsg, ...processedMessages],
      tools: getAllTools(),
      temperature: 0.1,
      max_tokens: 4096,
    };

    // Build headers — include Authorization if an API key is available
    const headers = { 'Content-Type': 'application/json' };
    const apiKey = process.env.OPENAI_API_KEY || process.env.ANTHROPIC_API_KEY || process.env.DEEPSEEK_API_KEY || config.model.apiKey;
    if (apiKey) {
      headers['Authorization'] = `Bearer ${apiKey}`;
    }
    // OpenRouter requires HTTP-Referer and X-Title headers
    if (baseUrl.includes('openrouter.ai')) {
      headers['HTTP-Referer'] = 'https://github.com/Doorman11991/smallcode';
      headers['X-Title'] = 'SmallCode';
    }

    const response = await fetch(`${baseUrl}/chat/completions`, {
      method: 'POST',
      headers,
      body: JSON.stringify(body),
    });

    if (!response.ok) {
      const err = await response.text();
      // Retry once on 4xx (handles LM Studio model reload / rate limit)
      if (response.status >= 400 && response.status < 500) {
        await new Promise(r => setTimeout(r, 2000));
        try {
          const retry = await fetch(`${baseUrl}/chat/completions`, {
            method: 'POST',
            headers,
            body: JSON.stringify(body),
          });
          if (retry.ok) return await retry.json();
        } catch {}
      }
      console.log(`  \x1b[31m✗ API error ${response.status}: ${err.slice(0, 200)}\x1b[0m`);
      return null;
    }

    const data = await response.json();

    // Track token usage
    if (tokenTracker && data?.usage) {
      tokenTracker.record(data, config.model.name);
    }

    // Auto-save session periodically
    if (sessionStore) {
      sessionStore.save(conversationHistory, {
        tokens: tokenTracker ? tokenTracker.stats() : undefined,
      });
      sessionStore.autoTitle(conversationHistory);
    }

    return data;
  } catch (err) {
    console.log(`  \x1b[31m✗ ${err.message}\x1b[0m`);
    return null;
  }
}

// Stream a final text response (no tools, just text output)
async function streamFinalResponse(config, messages) {
  const baseUrl = config.model.baseUrl;
  const systemMsg = {
    role: 'system',
    content: `You are SmallCode, a coding assistant. Summarize what you just did in 1-2 sentences. Be concise.`
  };

  try {
    const response = await fetch(`${baseUrl}/chat/completions`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        model: config.model.name,
        messages: [systemMsg, ...messages.slice(-6)], // Only last few messages for summary
        stream: true,
        temperature: 0.1,
        max_tokens: 256,
      }),
    });

    if (!response.ok) return null;

    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';
    let fullContent = '';

    // Start streaming display
    if (_fullscreenRef) _fullscreenRef.setStreaming(true);

    while (true) {
      const { done, value } = await reader.read();
      if (done) break;

      buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split('\n');
      buffer = lines.pop() || '';

      for (const line of lines) {
        if (!line.trim() || !line.startsWith('data: ')) continue;
        const data = line.slice(6);
        if (data === '[DONE]') {
          if (_fullscreenRef) { _fullscreenRef.endStream(); _fullscreenRef.setStreaming(false); }
          else console.log('');
          return fullContent;
        }
        try {
          const chunk = JSON.parse(data);
          const delta = chunk.choices?.[0]?.delta;
          if (delta?.content) {
            if (_fullscreenRef) {
              _fullscreenRef.streamToken(delta.content);
            } else {
              process.stdout.write(delta.content);
            }
            fullContent += delta.content;
          }
        } catch {}
      }
    }
    if (_fullscreenRef) { _fullscreenRef.endStream(); _fullscreenRef.setStreaming(false); }
    else console.log('');
    return fullContent;
  } catch {
    return null;
  }
}

// Streaming version for when no tools are needed (direct responses)
async function sendToModel(message, config) {
  const baseUrl = config.model.baseUrl || process.env.OLLAMA_HOST || 'http://localhost:11434';
  const systemPrompt = `You are SmallCode, a coding assistant. You help users by reading, editing, and creating code files.
Rules:
- Read files before editing them.
- Use search-and-replace for edits. Never rewrite entire files.
- Keep responses concise and focused.
- If a task is complex, break it into steps.`;

  // OpenAI-compatible (LM Studio, vLLM, etc.)
  if (config.model.provider === 'openai' || baseUrl.includes('/v1')) {
    try {
      const response = await fetch(`${baseUrl}/chat/completions`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          model: config.model.name,
          messages: [
            { role: 'system', content: systemPrompt },
            { role: 'user', content: message },
          ],
          stream: true,
          temperature: 0.1,
          max_tokens: 4096,
        }),
      });

      if (!response.ok) {
        const err = await response.text();
        console.log(`  ✗ LM Studio error: ${response.status} ${err.slice(0, 200)}`);
        return;
      }

      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = '';

      while (true) {
        const { done, value } = await reader.read();
        if (done) break;

        buffer += decoder.decode(value, { stream: true });
        const lines = buffer.split('\n');
        buffer = lines.pop() || '';

        for (const line of lines) {
          if (!line.trim() || !line.startsWith('data: ')) continue;
          const data = line.slice(6);
          if (data === '[DONE]') { console.log(''); return; }
          try {
            const chunk = JSON.parse(data);
            const delta = chunk.choices?.[0]?.delta;
            if (delta?.content) {
              process.stdout.write(delta.content);
            }
          } catch {}
        }
      }
      console.log('');
    } catch (err) {
      console.log(`  ✗ Error: ${err.message}`);
    }
    return;
  }

  // Ollama native endpoint
  try {
    const host = process.env.OLLAMA_HOST || 'http://localhost:11434';
    const response = await fetch(`${host}/api/chat`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        model: config.model.name,
        messages: [
          { role: 'system', content: systemPrompt },
          { role: 'user', content: message },
        ],
        stream: true,
      }),
    });

    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';

    while (true) {
      const { done, value } = await reader.read();
      if (done) break;

      buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split('\n');
      buffer = lines.pop() || '';

      for (const line of lines) {
        if (!line.trim()) continue;
        try {
          const chunk = JSON.parse(line);
          if (chunk.message && chunk.message.content) {
            process.stdout.write(chunk.message.content);
          }
          if (chunk.done) { console.log(''); return; }
        } catch {}
      }
    }
  } catch (err) {
    console.log(`  ✗ Error: ${err.message}`);
  }
}

// ─── Non-Interactive Mode ────────────────────────────────────────────────────

async function runNonInteractive(config, prompt) {
  if (!prompt) {
    // Read from stdin
    const chunks = [];
    for await (const chunk of process.stdin) {
      chunks.push(chunk);
    }
    prompt = Buffer.concat(chunks).toString().trim();
  }

  if (!prompt) {
    console.error('No prompt provided.');
    process.exit(1);
  }

  await runAgentLoop(prompt, config);
}

// ─── MCP Server Mode ─────────────────────────────────────────────────────────

function runMCP() {
  // Minimal MCP server implementation over stdio
  const rl = readline.createInterface({ input: process.stdin });

  rl.on('line', (line) => {
    try {
      const request = JSON.parse(line);
      const response = handleMCPRequest(request);
      console.log(JSON.stringify(response));
    } catch (err) {
      console.log(JSON.stringify({
        jsonrpc: '2.0',
        id: null,
        error: { code: -32700, message: 'Parse error' }
      }));
    }
  });
}

function handleMCPRequest(request) {
  const { id, method } = request;
  switch (method) {
    case 'initialize':
      return { jsonrpc: '2.0', id, result: {
        protocolVersion: '2024-11-05',
        capabilities: { tools: {} },
        serverInfo: { name: 'smallcode', version: VERSION },
      }};
    case 'tools/list':
      return { jsonrpc: '2.0', id, result: { tools: [
        { name: 'smallcode_read_file', description: 'Read file contents', inputSchema: { type: 'object', properties: { path: { type: 'string', description: 'File path' } }, required: ['path'] } },
        { name: 'smallcode_search', description: 'Search code with regex', inputSchema: { type: 'object', properties: { pattern: { type: 'string' }, path: { type: 'string' } }, required: ['pattern'] } },
        { name: 'smallcode_patch', description: 'Edit file via search-and-replace', inputSchema: { type: 'object', properties: { path: { type: 'string' }, old_str: { type: 'string' }, new_str: { type: 'string' } }, required: ['path', 'old_str', 'new_str'] } },
        { name: 'smallcode_bash', description: 'Run shell command', inputSchema: { type: 'object', properties: { command: { type: 'string' } }, required: ['command'] } },
        { name: 'smallcode_memory_load', description: 'Load relevant project memory for a task', inputSchema: { type: 'object', properties: { task: { type: 'string' } }, required: ['task'] } },
        { name: 'smallcode_memory_remember', description: 'Save knowledge to project memory', inputSchema: { type: 'object', properties: { type: { type: 'string' }, title: { type: 'string' }, content: { type: 'string' } }, required: ['type', 'title', 'content'] } },
        { name: 'smallcode_agent', description: 'Send a prompt to SmallCode agent', inputSchema: { type: 'object', properties: { message: { type: 'string' } }, required: ['message'] } },
      ]}};
    case 'tools/call':
      return handleMCPToolCall(id, request.params);
    default:
      return { jsonrpc: '2.0', id, error: { code: -32601, message: `Unknown method: ${method}` }};
  }
}

function handleMCPToolCall(id, params) {
  const { name, arguments: args } = params;
  let result = '';

  switch (name) {
    case 'smallcode_read_file':
      try { result = fs.readFileSync(path.resolve(args.path), 'utf-8'); }
      catch (e) { return { jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: `Error: ${e.message}` }], isError: true }}; }
      break;
    case 'smallcode_bash':
      try {
        const { execSync } = require('child_process');
        result = execSync(args.command, { encoding: 'utf-8', timeout: 30000, cwd: process.cwd() });
      } catch (e) { result = e.stdout || e.message; }
      break;
    case 'smallcode_search':
      try {
        const { execSync } = require('child_process');
        result = execSync(`rg --line-number --max-count 10 "${args.pattern}" ${args.path || '.'}`, { encoding: 'utf-8', timeout: 10000, cwd: process.cwd() });
      } catch { result = 'No matches'; }
      break;
    case 'smallcode_patch':
      try {
        const filePath = path.resolve(args.path);
        let content = fs.readFileSync(filePath, 'utf-8');
        if (!content.includes(args.old_str)) { result = 'Error: old_str not found'; break; }
        content = content.replace(args.old_str, args.new_str);
        fs.writeFileSync(filePath, content);
        result = `Patched ${args.path}`;
      } catch (e) { result = `Error: ${e.message}`; }
      break;
    case 'smallcode_memory_load': {
      const { objects, tokens_used } = memoryStore.loadForTask(args.task || '', 2000);
      result = objects.length > 0 
        ? objects.map(o => `[${o.type}] ${o.title}: ${o.content}`).join('\n\n')
        : 'No relevant memory found.';
      break;
    }
    case 'smallcode_memory_remember': {
      const obj = memoryStore.remember({ type: args.type || 'context', title: args.title || '', content: args.content || '', tags: args.tags || [] });
      result = obj.duplicate ? `Already known (${obj.existing_id})` : `Remembered: [${obj.type}] ${obj.title}`;
      break;
    }
    default:
      result = `Unknown tool: ${name}`;
  }

  return { jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: result }] }};
}

// ─── Main ────────────────────────────────────────────────────────────────────

async function main() {
  const config = loadConfig();

  // Check model is configured
  if (!config.model.name) {
    console.error('\n  ✗ No model configured.');
    console.error('  Set SMALLCODE_MODEL in .env, or add [model] name = "..." to smallcode.toml');
    console.error('  See .env.example for setup instructions.\n');
    process.exit(1);
  }

  // Initialize escalation engine
  escalationEngine = new EscalationEngine(config.escalation || {});

  // Initialize plugins and skills
  pluginLoader = new PluginLoader(process.cwd()).loadAll();
  skillManager = new SkillManager(process.cwd());

  // Initialize session + token tracking
  sessionStore = new SessionStore(process.cwd());
  tokenTracker = new TokenTracker();

  // Resume or create session
  if (flags.resume) {
    const resumed = sessionStore.resume();
    if (resumed) {
      conversationHistory.push(...resumed.messages);
    }
  }
  if (!sessionStore.current) {
    sessionStore.create(config.model.name);
  }

  if (flags.mcp) {
    runMCP();
    return;
  }

  if (flags.acp) {
    const { ACPAdapter } = require('../src/adapters/acp');
    const adapter = new ACPAdapter(runAgentLoop, config);
    adapter.start();
    return;
  }

  if (flags.nonInteractive || positional.length > 0) {
    await runNonInteractive(config, positional.join(' '));
    return;
  }

  await runTUI(config);
}

main().catch(err => {
  console.error(`Fatal: ${err.message}`);
  process.exit(1);
});

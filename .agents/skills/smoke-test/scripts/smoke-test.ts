#!/usr/bin/env node

import { spawn, type ChildProcess } from 'node:child_process';
import { createHash } from 'node:crypto';
import { access, mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { type AddressInfo, createServer } from 'node:net';
import { homedir } from 'node:os';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import dotenv from 'dotenv';

type CommandOptions = {
  cwd?: string;
  env?: NodeJS.ProcessEnv;
  input?: string;
  timeoutMs?: number;
};

type CommandResult = {
  code: number | null;
  output: string;
  signal: NodeJS.Signals | null;
  timedOut: boolean;
};

type SmokeTest = {
  name: string;
  run: () => Promise<void>;
  skip?: () => string | undefined;
};

const scriptPath = fileURLToPath(import.meta.url);
const repoRoot = resolve(dirname(scriptPath), '../../../..');
dotenv.config({ path: join(repoRoot, '.env.local'), quiet: true });
const throwawayRoot = resolve(process.env.LFS_CLOUD_SMOKE_THROWAWAY ?? join(homedir(), 'Sites', 'throwaway'));
const targetDir = join(repoRoot, 'target');
const configuredBinary = process.env.LFS_CLOUD_SMOKE_BINARY?.trim();
const binaryPath = configuredBinary
  ? resolve(repoRoot, configuredBinary)
  : join(targetDir, 'debug', process.platform === 'win32' ? 'lfscloud.exe' : 'lfscloud');
const pythonCommand = process.platform === 'win32' ? 'python' : 'python3';

if (configuredBinary !== undefined) process.env.LFS_CLOUD_SMOKE_BINARY = binaryPath;
const commandOutputLimit = 4 * 1024 * 1024;
const defaultTimeoutMs = 15 * 60 * 1000;
const githubCredentialEnv = 'LFS_CLOUD_GITHUB_TOKEN';
const driveCredentialEnvs = [
  'LFS_CLOUD_GOOGLE_DRIVE_CLIENT_ID',
  'LFS_CLOUD_GOOGLE_DRIVE_CLIENT_SECRET',
  'LFS_CLOUD_GOOGLE_DRIVE_REFRESH_TOKEN',
] as const;

let sandbox = '';
let currentChild: ChildProcess | undefined;

class SmokeFailure extends Error {
  readonly detail: string;

  constructor(message: string, detail = '') {
    super(message);
    this.detail = detail;
  }
}

function isEnabled(value: string | undefined): boolean {
  return ['1', 'true', 'yes'].includes(value?.toLowerCase() ?? '');
}

function terminateChild(child: ChildProcess, signal: NodeJS.Signals): void {
  if (child.pid === undefined) return;

  try {
    process.kill(-child.pid, signal);
  } catch {
    child.kill(signal);
  }
}

function appendOutput(current: string, chunk: Buffer | string): string {
  const combined = current + chunk.toString();
  if (combined.length <= commandOutputLimit) return combined;
  return `[output truncated]\n${combined.slice(-commandOutputLimit)}`;
}

function runCommand(command: string, args: string[], options: CommandOptions = {}): Promise<CommandResult> {
  return new Promise(complete => {
    const child = spawn(command, args, {
      cwd: options.cwd ?? repoRoot,
      detached: true,
      env: {
        ...process.env,
        ...options.env,
      },
      stdio: [options.input === undefined ? 'ignore' : 'pipe', 'pipe', 'pipe'],
    });
    currentChild = child;
    if (options.input !== undefined) child.stdin?.end(options.input);

    let output = '';
    let timedOut = false;
    let finished = false;
    let hardKill: NodeJS.Timeout | undefined;

    child.stdout?.on('data', chunk => {
      output = appendOutput(output, chunk);
    });
    child.stderr?.on('data', chunk => {
      output = appendOutput(output, chunk);
    });

    const timeout = setTimeout(() => {
      timedOut = true;
      terminateChild(child, 'SIGTERM');
      hardKill = setTimeout(() => terminateChild(child, 'SIGKILL'), 2_000);
    }, options.timeoutMs ?? defaultTimeoutMs);

    const finish = (result: CommandResult) => {
      if (finished) return;
      finished = true;
      clearTimeout(timeout);
      if (hardKill !== undefined) clearTimeout(hardKill);
      if (currentChild === child) currentChild = undefined;
      complete(result);
    };

    child.on('error', error => {
      finish({ code: null, output: appendOutput(output, error.message), signal: null, timedOut });
    });
    child.on('close', (code, signal) => {
      finish({ code, output, signal, timedOut });
    });
  });
}

async function command(executable: string, args: string[], options: CommandOptions = {}): Promise<string> {
  const result = await runCommand(executable, args, {
    ...options,
    env: {
      ...baseEnv(),
      ...options.env,
    },
  });

  if (result.code !== 0) {
    const outcome = result.timedOut ? 'timed out' : `exited with ${result.code ?? result.signal ?? 'an unknown error'}`;
    throw new SmokeFailure(`${executable} ${outcome}`, result.output);
  }

  return result.output;
}

function baseEnv(): NodeJS.ProcessEnv {
  return {
    CARGO_TARGET_DIR: targetDir,
    CARGO_TERM_COLOR: 'never',
    CLICOLOR: '0',
    GCM_INTERACTIVE: 'Never',
    GIT_TERMINAL_PROMPT: '0',
    LC_ALL: 'C',
    NO_COLOR: '1',
    RUST_BACKTRACE: '0',
  };
}

async function script(name: string, timeoutMs = defaultTimeoutMs, env: NodeJS.ProcessEnv = {}): Promise<void> {
  await command('bash', [join(repoRoot, 'scripts', 'manual', name)], { env, timeoutMs });
}

async function git(cwd: string, ...args: string[]): Promise<string> {
  return command('git', args, {
    cwd,
    env: {
      GIT_CONFIG_GLOBAL: join(sandbox, 'gitconfig'),
      GIT_CONFIG_NOSYSTEM: '1',
    },
  });
}

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new SmokeFailure(message);
}

async function pathExists(path: string): Promise<boolean> {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
}

async function initRepositorySmoke(): Promise<void> {
  const committedRepo = join(sandbox, 'init-committed');
  await mkdir(committedRepo);
  await git(committedRepo, 'init', '--quiet');
  await git(committedRepo, 'remote', 'add', 'origin', 'git@github.com:smoke-owner/smoke-repo.git');
  const output = await command(binaryPath, ['init', '--server', 'https://lfs.example.invalid'], { cwd: committedRepo });
  const lfsConfig = await readFile(join(committedRepo, '.lfsconfig'), 'utf8');
  assert(output.includes('.lfsconfig'), 'init did not report the committed config target');
  assert(
    lfsConfig.includes('url = https://lfs.example.invalid/github.com/smoke-owner/smoke-repo.git/info/lfs'),
    'init wrote an unexpected .lfsconfig route',
  );

  const localRepo = join(sandbox, 'init-local');
  await mkdir(localRepo);
  await git(localRepo, 'init', '--quiet');
  await git(localRepo, 'remote', 'add', 'origin', 'https://github.com/smoke-owner/local.git');
  await command(binaryPath, ['init', '--server', 'https://lfs.example.invalid', '--local'], { cwd: localRepo });
  const localUrl = await git(localRepo, 'config', '--local', '--get', 'lfs.url');
  assert(
    localUrl.trim() === 'https://lfs.example.invalid/github.com/smoke-owner/local.git/info/lfs',
    'init --local wrote an unexpected lfs.url',
  );
  assert(!(await pathExists(join(localRepo, '.lfsconfig'))), 'init --local created .lfsconfig');
}

async function migrationDryRunSmoke(): Promise<void> {
  const migrationRepo = join(sandbox, 'existing-lfs');
  const cacheRoot = join(sandbox, 'migration-cache');
  await mkdir(migrationRepo);
  await git(migrationRepo, 'init', '--quiet');
  await git(migrationRepo, 'config', 'user.name', 'LFS Cloud Smoke Test');
  await git(migrationRepo, 'config', 'user.email', 'smoke@example.invalid');
  await git(migrationRepo, 'config', 'commit.gpgSign', 'false');
  await git(migrationRepo, 'remote', 'add', 'origin', 'git@github.com:smoke-owner/existing-lfs.git');
  await git(migrationRepo, 'lfs', 'install', '--local');
  await git(migrationRepo, 'lfs', 'track', 'assets/*.bin');
  await mkdir(join(migrationRepo, 'assets'));
  await writeFile(join(migrationRepo, 'assets', 'fixture.bin'), 'deterministic lfs smoke payload\n');
  await git(migrationRepo, 'add', '.gitattributes', 'assets/fixture.bin');
  await git(migrationRepo, 'commit', '--quiet', '-m', 'Add existing LFS fixture');

  const statusBefore = await git(migrationRepo, 'status', '--porcelain=v1', '--untracked-files=all');
  const configBefore = await readFile(join(migrationRepo, '.git', 'config'), 'utf8');
  const output = await command(
    binaryPath,
    ['migrate', '--server', 'https://lfs.example.invalid', '--cache-root', cacheRoot, '--dry-run'],
    { cwd: migrationRepo },
  );
  const statusAfter = await git(migrationRepo, 'status', '--porcelain=v1', '--untracked-files=all');
  const configAfter = await readFile(join(migrationRepo, '.git', 'config'), 'utf8');

  assert(output.includes('lfscloud migrate dry-run'), 'migration did not render a dry-run report');
  assert(output.includes('mode: current-checkout'), 'migration did not use current-checkout scope');
  assert(output.includes('objects discovered: 1'), 'migration did not discover the LFS object');
  assert(statusAfter === statusBefore, 'migration dry-run changed worktree status');
  assert(configAfter === configBefore, 'migration dry-run changed Git config');
  assert(!(await pathExists(cacheRoot)), 'migration dry-run created cache state');
}

async function statusSmoke(): Promise<void> {
  const repo = join(sandbox, 'status-repo');
  const cacheRoot = join(sandbox, 'status-cache');
  const configPath = join(sandbox, 'status.yml');
  const gitConfig = join(sandbox, 'status-gitconfig');
  const credentialStore = join(sandbox, 'status-credentials');
  await mkdir(repo);
  await mkdir(join(cacheRoot, 'objects'), { recursive: true });
  await git(repo, 'init', '--quiet');
  await git(repo, 'remote', 'add', 'origin', 'git@github.com:smoke-owner/status.git');

  const server = createServer(socket => socket.end());
  await new Promise<void>((ready, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', ready);
  });

  try {
    const address = server.address() as AddressInfo;
    const serverUrl = `http://127.0.0.1:${address.port}`;
    const lfsUrl = `${serverUrl}/github.com/smoke-owner/status.git/info/lfs`;
    await writeFile(
      configPath,
      `server:\n  host: 127.0.0.1\n  port: ${address.port}\n  public_url: ${serverUrl}\n\nrepository_providers:\n  github-main:\n    type: github\n    api_url: https://api.github.com\n    oauth_client_id: smoke-client\n    oauth_client_secret: smoke-secret\n\nstorage_providers:\n  drive-smoke:\n    type: google_drive\n    credentials_ref: drive-smoke\n    root_folder_id: smoke-root\n\nrepositories:\n  - id: github-main:smoke-owner/status\n    repo_provider: github-main\n    host: github.com\n    owner: smoke-owner\n    name: status\n    provider_repository_id: "8675309"\n    storage_provider: drive-smoke\n`,
    );

    const gitEnv = {
      GIT_CONFIG_GLOBAL: gitConfig,
      GIT_CONFIG_NOSYSTEM: '1',
    };
    await command('git', ['config', '--global', 'credential.helper', `store --file=${credentialStore}`], {
      cwd: repo,
      env: gitEnv,
    });
    await command('git', ['config', '--local', `credential.${serverUrl}.useHttpPath`, 'true'], {
      cwd: repo,
      env: gitEnv,
    });
    await command('git', ['credential', 'approve'], {
      cwd: repo,
      env: gitEnv,
      input: `url=${lfsUrl}\nusername=lfscloud\npassword=smoke-status-token\n\n`,
    });

    const output = await command(binaryPath, ['--config', configPath, 'status', '--cache-root', cacheRoot], {
      cwd: repo,
      env: {
        ...gitEnv,
        LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_DRIVE_SMOKE:
          '{"client_id":"smoke-client","client_secret":"smoke-secret","refresh_token":"smoke-refresh"}',
      },
    });
    for (const marker of [
      'config     ok',
      'repository ok',
      'server     ok',
      `route      ok      ${lfsUrl}`,
      'mapping    ok      github-main:smoke-owner/status -> drive-smoke',
      'auth       ok      local LFS credential found',
      'storage    ok      google_drive drive-smoke credential is configured',
      'cache      ok',
    ]) {
      assert(output.includes(marker), `status output omitted: ${marker}`);
    }
    assert(!output.includes('smoke-status-token'), 'status output leaked the local token');
  } finally {
    await new Promise<void>(closed => server.close(() => closed()));
  }
}

async function gcSmoke(): Promise<void> {
  const repo = join(sandbox, 'gc-repo');
  const cacheRoot = join(sandbox, 'gc-cache');
  const payload = Buffer.from('deterministic unreferenced cache object\n');
  const oid = createHash('sha256').update(payload).digest('hex');
  const objectPath = join(cacheRoot, 'objects', oid.slice(0, 2), oid.slice(2, 4), oid);
  await mkdir(repo);
  await mkdir(dirname(objectPath), { recursive: true });
  await writeFile(objectPath, payload);
  await git(repo, 'init', '--quiet');
  await git(repo, 'remote', 'add', 'origin', 'git@github.com:smoke-owner/gc.git');

  const dryRun = await command(binaryPath, ['gc', '--cache-root', cacheRoot, '--dry-run'], {
    cwd: repo,
  });
  assert(dryRun.includes(oid), 'gc dry-run omitted the unreferenced object');
  assert(await pathExists(objectPath), 'gc dry-run removed an object');

  const output = await command(binaryPath, ['gc', '--cache-root', cacheRoot], { cwd: repo });
  assert(output.includes(oid), 'gc output omitted the removed object');
  assert(!(await pathExists(objectPath)), 'gc retained an unreferenced object');
}

function enabledFlag(name: string, description: string): () => string | undefined {
  return () => (isEnabled(process.env[name]) ? undefined : `${description}; set ${name}=1 to enable`);
}

function hasCredential(name: string): boolean {
  return (process.env[name]?.trim().length ?? 0) > 0;
}

function hasDriveCredential(): boolean {
  return driveCredentialEnvs.every(hasCredential);
}

function missingCredential(available: () => boolean, description: string): () => string | undefined {
  return () => (available() ? undefined : description);
}

function cargoTestsAlreadyRan(): string | undefined {
  return isEnabled(process.env.LFS_CLOUD_SMOKE_SKIP_CARGO_TESTS)
    ? 'Cargo tests already ran before the release build'
    : undefined;
}

function tests(): SmokeTest[] {
  return [
    {
      name: 'toolchain prerequisites',
      run: async () => {
        await command('node', ['--version']);
        await command('cargo', ['--version']);
        await command('git', ['--version']);
        await command('git', ['lfs', 'version']);
        await command(pythonCommand, ['--version']);
        await command('curl', ['--version']);
      },
    },
    {
      name: 'build and CLI surface',
      run: async () => {
        if (configuredBinary === undefined) {
          await command('cargo', ['build', '--quiet'], { timeoutMs: 30 * 60 * 1000 });
        } else {
          assert(await pathExists(binaryPath), `configured smoke binary does not exist: ${binaryPath}`);
        }
        const help = await command(binaryPath, ['--help']);
        const version = await command(binaryPath, ['--version']);
        for (const subcommand of [
          'serve',
          'login',
          'logout',
          'init',
          'status',
          'pull',
          'hydrate',
          'dehydrate',
          'gc',
          'migrate',
        ]) {
          assert(help.includes(subcommand), `CLI help omitted ${subcommand}`);
        }
        assert(version.startsWith('lfscloud '), 'CLI version output was unexpected');
      },
    },
    {
      name: 'automated Rust targets',
      skip: cargoTestsAlreadyRan,
      run: async () => {
        await command('cargo', ['test', '--all-targets'], { timeoutMs: 45 * 60 * 1000 });
      },
    },
    {
      name: 'Rust documentation tests',
      skip: cargoTestsAlreadyRan,
      run: async () => {
        await command('cargo', ['test', '--doc'], { timeoutMs: 30 * 60 * 1000 });
      },
    },
    { name: 'repository initialization', run: initRepositorySmoke },
    { name: 'existing Git LFS migration dry-run', run: migrationDryRunSmoke },
    { name: 'Git credential approval', run: () => script('verify-git-credential-approve.sh') },
    {
      name: 'credential-helper fallback',
      run: () => script('verify-git-credential-helper-fallback.sh'),
    },
    { name: 'login workflow', run: () => script('verify-login-command.sh') },
    { name: 'logout workflow', run: () => script('verify-logout-command.sh') },
    { name: 'repository status workflow', run: statusSmoke },
    { name: 'hydrate and dehydrate workflows', run: () => script('verify-local-cache-cli.sh') },
    {
      name: 'cache materialization',
      run: () => script('verify-local-cache-materialization.sh'),
    },
    { name: 'pull workflow', run: () => script('verify-pull-command.sh') },
    { name: 'garbage collection workflow', run: gcSmoke },
    {
      name: 'migration source fetch',
      run: () => script('verify-migration-source-fetch.sh'),
    },
    {
      name: 'migration upload simulation',
      run: () => script('verify-migration-upload-simulation.sh'),
    },
    {
      name: 'secret redaction regressions',
      run: () => script('verify-secret-redaction.sh', 45 * 60 * 1000),
    },
    {
      name: 'LAN server route',
      skip: enabledFlag('LFS_CLOUD_RUN_LAN_SMOKE', 'requires an intentional LAN bind/config'),
      run: () => script('verify-lan-smoke-test.sh', 30 * 60 * 1000),
    },
    {
      name: 'GitHub disposable repository',
      skip: missingCredential(() => hasCredential(githubCredentialEnv), `requires ${githubCredentialEnv}`),
      run: () =>
        script('verify-github-integration.sh', 30 * 60 * 1000, {
          LFS_CLOUD_RUN_GITHUB_INTEGRATION: '1',
        }),
    },
    {
      name: 'Google Drive disposable folder',
      skip: missingCredential(hasDriveCredential, `requires ${driveCredentialEnvs.join(', ')}`),
      run: () =>
        script('verify-google-drive-integration.sh', 30 * 60 * 1000, {
          LFS_CLOUD_RUN_GOOGLE_DRIVE_INTEGRATION: '1',
        }),
    },
    {
      name: 'black-box Git LFS live transfer',
      skip: missingCredential(
        () => hasCredential(githubCredentialEnv) && hasDriveCredential(),
        `requires ${githubCredentialEnv} and ${driveCredentialEnvs.join(', ')}`,
      ),
      run: () =>
        script('verify-live-provider-transfer.sh', 45 * 60 * 1000, {
          LFS_CLOUD_RUN_LIVE_TRANSFER_INTEGRATION: '1',
        }),
    },
  ];
}

function sanitizeDetail(detail: string): string {
  const replacements = [
    [sandbox, '<sandbox>'],
    [repoRoot, '<repo>'],
    [throwawayRoot, '<throwaway>'],
  ] as const;
  let sanitized = detail.trim();
  for (const [value, replacement] of replacements) {
    if (value) sanitized = sanitized.split(value).join(replacement);
  }
  const lines = sanitized.split('\n');
  return lines.slice(-40).join('\n');
}

async function cleanup(): Promise<void> {
  if (!sandbox) return;
  const relativeSandbox = relative(throwawayRoot, sandbox);
  if (relativeSandbox.startsWith('..') || !relativeSandbox.startsWith('.lfscloud-smoke-')) {
    throw new Error(`refusing to remove unexpected smoke path: ${sandbox}`);
  }
  await rm(sandbox, { recursive: true, force: true });
}

async function handleSignal(signal: NodeJS.Signals): Promise<void> {
  if (currentChild !== undefined) terminateChild(currentChild, signal);
  await cleanup();
  process.exit(signal === 'SIGINT' ? 130 : 143);
}

async function main(): Promise<void> {
  assert(await pathExists(join(repoRoot, 'Cargo.toml')), 'run the skill from an LFS Cloud checkout');
  assert(await pathExists(join(throwawayRoot, '.git')), `throwaway repository not found at ${throwawayRoot}`);

  sandbox = await mkdtemp(join(throwawayRoot, '.lfscloud-smoke-'));
  process.once('SIGINT', () => void handleSignal('SIGINT'));
  process.once('SIGTERM', () => void handleSignal('SIGTERM'));

  let passed = 0;
  let failed = 0;
  let skipped = 0;

  console.log('LFS Cloud smoke test');
  for (const test of tests()) {
    const skipReason = test.skip?.();
    if (skipReason !== undefined) {
      skipped += 1;
      console.log(`[SKIP] ${test.name} - ${skipReason}`);
      continue;
    }

    try {
      await test.run();
      passed += 1;
      console.log(`[PASS] ${test.name}`);
    } catch (error) {
      failed += 1;
      const failure = error instanceof SmokeFailure ? error : new SmokeFailure(String(error));
      console.log(`[FAIL] ${test.name} - ${failure.message}`);
      const detail = sanitizeDetail(failure.detail);
      if (detail) console.log(detail);
    }
  }

  console.log(`Summary: ${passed} passed, ${failed} failed, ${skipped} skipped`);
  if (failed > 0) process.exitCode = 1;
}

try {
  await main();
} catch (error) {
  const message = error instanceof Error ? error.message : String(error);
  console.error(`[FAIL] smoke-test setup - ${message}`);
  process.exitCode = 1;
} finally {
  await cleanup();
}

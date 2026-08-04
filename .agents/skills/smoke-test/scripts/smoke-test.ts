#!/usr/bin/env node

import { spawn, type ChildProcess } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync } from 'node:fs';
import { access, mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { createServer as createHttpServer, get as httpGet, type IncomingMessage, type ServerResponse } from 'node:http';
import { type AddressInfo, createServer as createNetServer } from 'node:net';
import { homedir } from 'node:os';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
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

type CommandInvocation = {
  executable: string;
  args: string[];
};

type BackgroundCommand = {
  output: () => string;
  result: Promise<CommandResult>;
  stop: () => Promise<CommandResult>;
};

const scriptPath = fileURLToPath(import.meta.url);
const repoRoot = resolve(dirname(scriptPath), '../../../..');
dotenv.config({ path: join(repoRoot, '.env.local'), quiet: true });

function defaultThrowawayRoot(homeDirectory: string, platform: NodeJS.Platform): string {
  const projectsDirectory = platform === 'win32' ? 'Projects' : 'Sites';
  return join(homeDirectory, projectsDirectory, 'throwaway');
}

function gcloudInvocation(
  args: string[],
  platform: NodeJS.Platform,
  commandShell = process.env.ComSpec?.trim() || 'cmd.exe',
): CommandInvocation {
  return platform === 'win32'
    ? {
        executable: commandShell,
        args: ['/d', '/s', '/c', 'gcloud.cmd', ...args],
      }
    : {
        executable: 'gcloud',
        args,
      };
}

const throwawayRoot = resolve(
  process.env.LFS_CLOUD_SMOKE_THROWAWAY ?? defaultThrowawayRoot(homedir(), process.platform),
);
const targetDir = join(repoRoot, 'target');
const configuredBinary = process.env.LFS_CLOUD_SMOKE_BINARY?.trim();
const binaryPath = configuredBinary
  ? resolve(repoRoot, configuredBinary)
  : join(targetDir, 'debug', process.platform === 'win32' ? 'lfscloud.exe' : 'lfscloud');
const pythonCommand = process.platform === 'win32' ? 'python' : 'python3';

if (configuredBinary !== undefined) process.env.LFS_CLOUD_SMOKE_BINARY = binaryPath;
const commandOutputLimit = 4 * 1024 * 1024;
const defaultTimeoutMs = 15 * 60 * 1000;
const githubPatEnv = 'LFS_CLOUD_GITHUB_PAT';
const driveConfigDirEnv = 'LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR';

let sandbox = '';
let currentChild: ChildProcess | undefined;
const backgroundCommands = new Set<BackgroundCommand>();

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

function runBackgroundCommand(command: string, args: string[], options: CommandOptions = {}): BackgroundCommand {
  const child = spawn(command, args, {
    cwd: options.cwd ?? repoRoot,
    detached: true,
    env: {
      ...process.env,
      ...baseEnv(),
      ...options.env,
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let output = '';
  let finished = false;
  let completeResult: (result: CommandResult) => void = () => undefined;
  let background: BackgroundCommand;
  const result = new Promise<CommandResult>(complete => {
    completeResult = complete;
  });
  const finish = (completed: CommandResult) => {
    if (finished) return;
    finished = true;
    backgroundCommands.delete(background);
    completeResult(completed);
  };
  child.stdout?.on('data', chunk => {
    output = appendOutput(output, chunk);
  });
  child.stderr?.on('data', chunk => {
    output = appendOutput(output, chunk);
  });
  child.on('error', error => {
    finish({
      code: null,
      output: appendOutput(output, error.message),
      signal: null,
      timedOut: false,
    });
  });
  child.on('close', (code, signal) => {
    finish({ code, output, signal, timedOut: false });
  });

  background = {
    output: () => output,
    result,
    stop: async () => {
      if (!finished) terminateChild(child, 'SIGTERM');
      const hardKill = setTimeout(() => {
        if (!finished) terminateChild(child, 'SIGKILL');
      }, 2_000);
      const completed = await result;
      clearTimeout(hardKill);
      return completed;
    },
  };
  backgroundCommands.add(background);
  return background;
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

async function gcloud(args: string[]): Promise<string> {
  const invocation = gcloudInvocation(args, process.platform);
  return command(invocation.executable, invocation.args);
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

function isolatedGitEnv(): NodeJS.ProcessEnv {
  return {
    GIT_CONFIG_GLOBAL: join(sandbox, 'gitconfig'),
    GIT_CONFIG_NOSYSTEM: '1',
  };
}

async function git(cwd: string, ...args: string[]): Promise<string> {
  return command('git', args, {
    cwd,
    env: isolatedGitEnv(),
  });
}

async function gitWithEnv(cwd: string, env: NodeJS.ProcessEnv, ...args: string[]): Promise<string> {
  return command('git', args, { cwd, env });
}

async function requestBody(request: IncomingMessage): Promise<Buffer> {
  const maxBytes = 4 * 1024 * 1024;
  const chunks: Buffer[] = [];
  let totalBytes = 0;
  for await (const chunk of request) {
    const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    totalBytes += bytes.length;
    if (totalBytes > maxBytes) throw new Error(`smoke request body exceeded ${maxBytes} bytes`);
    chunks.push(bytes);
  }
  return Buffer.concat(chunks);
}

function jsonResponse(response: ServerResponse, status: number, body: unknown): void {
  const bytes = Buffer.from(JSON.stringify(body));
  response.writeHead(status, {
    'Content-Length': bytes.length,
    'Content-Type': 'application/vnd.git-lfs+json',
  });
  response.end(bytes);
}

function lfsMediaPath(repository: string, oid: string): string {
  return join(repository, '.git', 'lfs', 'objects', oid.slice(0, 2), oid.slice(2, 4), oid);
}

async function writeLfsMediaObject(repository: string, oid: string, bytes: Buffer): Promise<void> {
  const path = lfsMediaPath(repository, oid);
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, bytes);
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

function sleep(milliseconds: number): Promise<void> {
  return new Promise(resolveSleep => setTimeout(resolveSleep, milliseconds));
}

function requestHttpStatus(url: string): Promise<number | undefined> {
  return new Promise(resolveStatus => {
    const request = httpGet(url, response => {
      response.resume();
      resolveStatus(response.statusCode);
    });
    request.setTimeout(1_000, () => request.destroy());
    request.on('error', () => resolveStatus(undefined));
  });
}

async function waitForHttpStatus(url: string, expectedStatus: number, server: BackgroundCommand): Promise<void> {
  const deadline = Date.now() + 20_000;
  while (Date.now() < deadline) {
    const completed = await Promise.race([server.result.then(result => ({ result })), sleep(0).then(() => undefined)]);
    if (completed !== undefined) {
      throw new SmokeFailure('lfscloud serve exited before becoming ready', completed.result.output);
    }
    if ((await requestHttpStatus(url)) === expectedStatus) return;
    await sleep(250);
  }
  throw new SmokeFailure(`lfscloud serve did not return HTTP ${expectedStatus} for ${url}`, server.output());
}

async function availablePort(host: string, port: number): Promise<number> {
  const server = createNetServer();
  await new Promise<void>((ready, reject) => {
    server.once('error', reject);
    server.listen(port, host, ready);
  }).catch(error => {
    throw new SmokeFailure(`TCP port ${host}:${port} is unavailable`, String(error));
  });
  const address = server.address() as AddressInfo;
  await new Promise<void>((closed, reject) => server.close(error => (error === undefined ? closed() : reject(error))));
  return address.port;
}

function assertExpectedServerStop(result: CommandResult): void {
  assert(!result.timedOut, 'background lfscloud server timed out during shutdown');
  assert(
    result.code === 0 || result.signal === 'SIGTERM',
    `background lfscloud server stopped unexpectedly: ${result.output}`,
  );
}

async function defaultServerStartupSmoke(): Promise<void> {
  const configPath = join(sandbox, 'default-server.yml');
  await availablePort('0.0.0.0', 15_370);
  await writeFile(configPath, 'server: {}\n');
  const server = runBackgroundCommand(binaryPath, ['--config', configPath, 'serve']);
  let stopped: CommandResult | undefined;
  try {
    await waitForHttpStatus('http://127.0.0.1:15370/status', 404, server);
    assert(
      server.output().includes('local:   http://127.0.0.1:15370'),
      'default server startup did not advertise the loopback URL on port 15370',
    );
    assert(server.output().includes('network:'), 'default server startup omitted network reachability output');
  } finally {
    stopped = await server.stop();
  }
  assert(stopped !== undefined, 'default server shutdown result was unavailable');
  assertExpectedServerStop(stopped);
}

async function sessionKeyRotationSafetySmoke(): Promise<void> {
  const missingConfig = join(sandbox, 'missing-session-config.yml');
  const cancelled = await runCommand(binaryPath, ['--config', missingConfig, 'sessions', 'generate-key'], {
    env: baseEnv(),
    input: 'no\n',
  });
  assert(cancelled.code === 0, 'declined session-key rotation did not exit successfully');
  assert(cancelled.output.includes('invalidate all current'), 'rotation confirmation omitted invalidation warning');
  assert(cancelled.output.includes('was not changed'), 'declined rotation did not report cancellation');
  assert(!(await pathExists(missingConfig)), 'declined rotation touched a missing config path');

  const explicitConfig = join(sandbox, 'explicit-session-secret.yml');
  await writeFile(
    explicitConfig,
    'server:\n  session_encryption_secret: smoke-explicit-session-secret-at-least-32-characters\n',
  );
  const explicit = await runCommand(binaryPath, ['--config', explicitConfig, 'sessions', 'generate-key'], {
    env: baseEnv(),
    input: 'yes\n',
  });
  assert(explicit.code !== 0, 'managed rotation unexpectedly accepted an explicit session secret');
  assert(
    explicit.output.includes('manages only the native credential-store key'),
    'explicit-secret rotation failure omitted the authoritative-key explanation',
  );

  const lockedConfig = join(sandbox, 'locked-session-key.yml');
  const lockedMetadata = join(sandbox, 'locked-session-metadata.sqlite3');
  const port = await availablePort('0.0.0.0', 0);
  await writeFile(lockedConfig, `server:\n  port: ${port}\n  metadata_path: ${JSON.stringify(lockedMetadata)}\n`);
  const server = runBackgroundCommand(binaryPath, ['--config', lockedConfig, 'serve']);
  let stopped: CommandResult | undefined;
  try {
    await waitForHttpStatus(`http://127.0.0.1:${port}/status`, 404, server);
    const locked = await runCommand(binaryPath, ['--config', lockedConfig, 'sessions', 'generate-key'], {
      env: baseEnv(),
      input: 'yes\n',
    });
    assert(locked.code !== 0, 'session-key rotation unexpectedly ran while the server was active');
    assert(locked.output.includes('already running'), 'active-server rotation failure omitted lock guidance');
  } finally {
    stopped = await server.stop();
  }
  assert(stopped !== undefined, 'session-key safety server shutdown result was unavailable');
  assertExpectedServerStop(stopped);
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

async function configurationCommandsSmoke(): Promise<void> {
  const configPath = join(sandbox, 'configuration-commands.yml');
  await writeFile(configPath, 'server: {}\n');

  await command(binaryPath, [
    '--config',
    configPath,
    'config',
    'repository',
    'add',
    '--id',
    'github-main',
    '--type',
    'github',
  ]);
  await command(binaryPath, [
    '--config',
    configPath,
    'config',
    'storage',
    'add',
    '--id',
    'drive-main',
    '--type',
    'google-drive',
    '--credentials-type',
    'gcloud',
    '--config-dir',
    join(sandbox, 'configuration-gcloud'),
    '--executable',
    process.platform === 'win32' ? 'gcloud.cmd' : 'gcloud',
    '--root-folder-id',
    'smoke-root',
    '--display-name',
    'Smoke Drive',
  ]);
  await command(binaryPath, [
    '--config',
    configPath,
    'repository',
    'add',
    '--id',
    'github-main:smoke-owner/smoke-repo',
    '--repo-provider',
    'github-main',
    '--host',
    'github.com',
    '--owner',
    'smoke-owner',
    '--name',
    'smoke-repo',
    '--provider-repository-id',
    '123456789',
    '--storage-provider',
    'drive-main',
  ]);

  const updated = await command(binaryPath, [
    '--config',
    configPath,
    'repository',
    'add',
    '--id',
    'github-main:smoke-owner/smoke-repo',
    '--name',
    'smoke-renamed',
  ]);
  const unchanged = await command(binaryPath, [
    '--config',
    configPath,
    'repository',
    'add',
    '--id',
    'github-main:smoke-owner/smoke-repo',
    '--name',
    'smoke-renamed',
  ]);
  assert(updated.includes('updated repository'), 'partial repository add did not update the existing mapping');
  assert(unchanged.includes('unchanged repository'), 'repeated repository add was not idempotent');

  const repositoryProviders = await command(binaryPath, ['--config', configPath, 'config', 'repository', 'list']);
  const storageProviders = await command(binaryPath, ['--config', configPath, 'config', 'storage', 'list']);
  const repositories = await command(binaryPath, ['--config', configPath, 'repository', 'list']);
  assert(repositoryProviders.includes('github-main'), 'repository-provider list omitted the configured provider');
  assert(
    repositoryProviders.includes('https://api.github.com'),
    'repository-provider list omitted the effective public GitHub API default',
  );
  assert(repositoryProviders.includes('LEGACY SESSION SECRET'), 'repository-provider list omitted legacy auth heading');
  assert(
    !repositoryProviders.includes('configured'),
    'new repository provider unexpectedly configured a server-owned PAT',
  );
  const configuredYaml = await readFile(configPath, 'utf8');
  assert(!configuredYaml.includes('api_url'), 'configuration command persisted the default GitHub API URL');
  assert(!configuredYaml.includes('personal_access_token'), 'configuration command wrote a server-owned GitHub PAT');
  assert(storageProviders.includes('drive-main'), 'storage-provider list omitted the configured provider');
  assert(storageProviders.includes('Smoke Drive'), 'storage-provider list omitted the display name');
  assert(repositories.includes('smoke-renamed'), 'repository list omitted the partially updated mapping');

  await command(binaryPath, [
    '--config',
    configPath,
    'repository',
    'remove',
    '--id',
    'github-main:smoke-owner/smoke-repo',
  ]);
  const repeatedRemove = await command(binaryPath, [
    '--config',
    configPath,
    'repository',
    'remove',
    '--id',
    'github-main:smoke-owner/smoke-repo',
  ]);
  assert(repeatedRemove.includes('not found repository'), 'repeated repository remove was not idempotent');
  await command(binaryPath, ['--config', configPath, 'config', 'storage', 'remove', '--id', 'drive-main']);
  await command(binaryPath, ['--config', configPath, 'config', 'repository', 'remove', '--id', 'github-main']);

  const interactivePath = join(sandbox, 'interactive-configuration-commands.yml');
  await writeFile(interactivePath, 'server: {}\n');
  await command(binaryPath, ['--config', interactivePath, 'config', 'repository', 'add'], {
    input: 'github-interactive\n\n\n',
  });
  const interactiveYaml = await readFile(interactivePath, 'utf8');
  assert(
    !interactiveYaml.includes('personal_access_token'),
    'interactive provider add wrote a server-owned GitHub PAT',
  );
  await command(binaryPath, ['--config', interactivePath, 'config', 'storage', 'add'], {
    input: `drive-interactive\n\n\n${join(sandbox, 'interactive-gcloud')}\n\ninteractive-root\nInteractive Drive\n`,
  });
  await command(binaryPath, ['--config', interactivePath, 'repository', 'add'], {
    input: 'github-interactive:owner/repo\ngithub-interactive\n\nowner\nrepo\n987654321\ndrive-interactive\n',
  });
  const interactiveRepositories = await command(binaryPath, ['--config', interactivePath, 'repository', 'list']);
  assert(
    interactiveRepositories.includes('github-interactive:owner/repo'),
    'interactive repository add did not create a listable mapping',
  );
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
  await writeFile(join(migrationRepo, 'assets', 'fixture.bin'), 'historical lfs smoke payload\n');
  await git(migrationRepo, 'add', '.gitattributes', 'assets/fixture.bin');
  await git(migrationRepo, 'commit', '--quiet', '-m', 'Add first LFS fixture version');
  await writeFile(join(migrationRepo, 'assets', 'fixture.bin'), 'latest lfs smoke payload with changed bytes\n');
  await git(migrationRepo, 'add', 'assets/fixture.bin');
  await git(migrationRepo, 'commit', '--quiet', '-m', 'Change LFS fixture bytes');

  const statusBefore = await git(migrationRepo, 'status', '--porcelain=v1', '--untracked-files=all');
  const configBefore = await readFile(join(migrationRepo, '.git', 'config'), 'utf8');
  const output = await command(
    binaryPath,
    ['migrate', '--server', 'https://lfs.example.invalid', '--cache-root', cacheRoot, '--all-refs', '--dry-run'],
    { cwd: migrationRepo, env: isolatedGitEnv() },
  );
  const statusAfter = await git(migrationRepo, 'status', '--porcelain=v1', '--untracked-files=all');
  const configAfter = await readFile(join(migrationRepo, '.git', 'config'), 'utf8');

  assert(output.includes('lfscloud migrate dry-run'), 'migration did not render a dry-run report');
  assert(output.includes('mode: all-refs'), 'migration did not use complete-history scope');
  assert(output.includes('objects discovered: 2'), 'migration did not discover both historical LFS objects');
  assert(statusAfter === statusBefore, 'migration dry-run changed worktree status');
  assert(configAfter === configBefore, 'migration dry-run changed Git config');
  assert(!(await pathExists(cacheRoot)), 'migration dry-run created cache state');
}

async function mockServerMediatedFollowUpMigrationSmoke(): Promise<void> {
  const remoteUrl = 'https://github.com/smoke-owner/followup.git';
  const targetRoute = '/github.com/smoke-owner/followup.git/info/lfs';
  const sourceObjects = new Map<string, Buffer>();
  const targetObjects = new Map<string, Buffer>();
  const sourceRequests = new Set<string>();
  const loginUsers = new Set<string>();
  const targetUploads: Array<{ actor: string; oid: string }> = [];
  const targetBatches: Array<{ actor: string; oids: string[] }> = [];
  let serverBase = '';
  let serverFailure: string | undefined;
  const uploadAuthorization = (actor: string): string =>
    `Basic ${Buffer.from(`lfscloud:lfs-session-${actor}`, 'utf8').toString('base64')}`;

  const server = createHttpServer((request, response) => {
    void (async () => {
      const url = new URL(request.url ?? '/', serverBase || 'http://127.0.0.1');
      const authorization = request.headers.authorization ?? '';

      if (request.method === 'POST' && url.pathname === '/auth/github/pat') {
        const sessions: Record<string, { actor: string; token: string }> = {
          'Bearer github_pat_smoke_user_a': { actor: 'user-a', token: 'lfs-session-user-a' },
          'Bearer github_pat_smoke_user_b': { actor: 'user-b', token: 'lfs-session-user-b' },
        };
        const session = sessions[authorization];
        if (session === undefined) {
          jsonResponse(response, 401, { message: 'unknown smoke user' });
          return;
        }
        loginUsers.add(session.actor);
        jsonResponse(response, 200, { lfs_token: session.token });
        return;
      }

      if (request.method === 'POST' && url.pathname === '/legacy/objects/batch') {
        const body = JSON.parse((await requestBody(request)).toString('utf8')) as {
          objects?: Array<{ oid: string; size: number }>;
        };
        const objects = (body.objects ?? []).map(object => {
          sourceRequests.add(object.oid);
          const bytes = sourceObjects.get(object.oid);
          return bytes === undefined
            ? { oid: object.oid, size: object.size, error: { code: 404, message: 'missing source object' } }
            : {
                oid: object.oid,
                size: object.size,
                actions: {
                  download: {
                    href: `${serverBase}/legacy/objects/${object.oid}`,
                    header: {},
                  },
                },
              };
        });
        jsonResponse(response, 200, { transfer: 'basic', objects });
        return;
      }

      if (request.method === 'GET' && url.pathname.startsWith('/legacy/objects/')) {
        const oid = url.pathname.slice('/legacy/objects/'.length);
        const bytes = sourceObjects.get(oid);
        if (bytes === undefined) {
          response.writeHead(404).end();
          return;
        }
        response.writeHead(200, { 'Content-Length': bytes.length, 'Content-Type': 'application/octet-stream' });
        response.end(bytes);
        return;
      }

      if (request.method === 'POST' && url.pathname === `${targetRoute}/objects/batch`) {
        const actors: Record<string, string> = {
          'Bearer lfs-session-user-a': 'user-a',
          'Bearer lfs-session-user-b': 'user-b',
        };
        const actor = actors[authorization];
        if (actor === undefined) {
          jsonResponse(response, 403, { message: 'write access required' });
          return;
        }
        const body = JSON.parse((await requestBody(request)).toString('utf8')) as {
          objects?: Array<{ oid: string; size: number }>;
        };
        const requested = body.objects ?? [];
        if (requested.length > 0) targetBatches.push({ actor, oids: requested.map(object => object.oid) });
        const objects = requested.map(object => ({
          oid: object.oid,
          size: object.size,
          actions: targetObjects.has(object.oid)
            ? {}
            : {
                upload: {
                  href: `${serverBase}${targetRoute}/objects/${object.oid}?size=${object.size}`,
                  header: { Authorization: uploadAuthorization(actor) },
                },
              },
        }));
        jsonResponse(response, 200, { transfer: 'basic', objects });
        return;
      }

      const targetObjectPrefix = `${targetRoute}/objects/`;
      if (request.method === 'PUT' && url.pathname.startsWith(targetObjectPrefix)) {
        const actors: Record<string, string> = {
          [uploadAuthorization('user-a')]: 'user-a',
          [uploadAuthorization('user-b')]: 'user-b',
        };
        const actor = actors[authorization];
        if (actor === undefined) {
          jsonResponse(response, 403, { message: 'write access required' });
          return;
        }
        const oid = url.pathname.slice(targetObjectPrefix.length);
        const bytes = await requestBody(request);
        assert(
          createHash('sha256').update(bytes).digest('hex') === oid,
          'migration uploaded bytes under the wrong OID',
        );
        assert(Number(url.searchParams.get('size')) === bytes.length, 'migration upload size did not match its action');
        targetObjects.set(oid, bytes);
        targetUploads.push({ actor, oid });
        response.writeHead(200, { 'Content-Length': 0 }).end();
        return;
      }

      response.writeHead(404, { 'Content-Length': 0 }).end();
    })().catch(error => {
      serverFailure = error instanceof Error ? error.message : String(error);
      if (!response.headersSent) jsonResponse(response, 500, { message: 'smoke server failure' });
      else response.destroy();
    });
  });

  await new Promise<void>((ready, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', ready);
  });

  try {
    const address = server.address() as AddressInfo;
    serverBase = `http://127.0.0.1:${address.port}`;
    const sourceLfsUrl = `${serverBase}/legacy`;
    const targetLfsUrl = `${serverBase}${targetRoute}`;
    const central = join(sandbox, 'followup-central.git');
    const centralUrl = pathToFileURL(central).toString();
    const seed = join(sandbox, 'followup-seed');
    const userA = join(sandbox, 'followup-user-a');
    const userB = join(sandbox, 'followup-user-b');
    const payloadA = Buffer.from('centrally migrated LFS object\n');
    const payloadB = Buffer.from('follow-up user LFS object\n');
    const oidA = createHash('sha256').update(payloadA).digest('hex');
    const oidB = createHash('sha256').update(payloadB).digest('hex');
    sourceObjects.set(oidA, payloadA);
    sourceObjects.set(oidB, payloadB);

    const userEnv = (name: string): NodeJS.ProcessEnv => ({
      GIT_CONFIG_GLOBAL: join(sandbox, `${name}-gitconfig`),
      GIT_CONFIG_NOSYSTEM: '1',
    });
    const seedEnv = userEnv('seed');
    const userAEnv = userEnv('user-a');
    const userBEnv = userEnv('user-b');
    const configureEnvironment = async (env: NodeJS.ProcessEnv, credentialStore?: string): Promise<void> => {
      await command('git', ['config', '--global', `url.${centralUrl}.insteadOf`, remoteUrl], { env });
      await command('git', ['config', '--global', 'protocol.file.allow', 'always'], { env });
      if (credentialStore !== undefined) {
        await command('git', ['config', '--global', 'credential.helper', `store --file=${credentialStore}`], { env });
      }
    };
    const initializeCheckout = async (directory: string, branch: string, env: NodeJS.ProcessEnv): Promise<void> => {
      await mkdir(directory);
      await gitWithEnv(directory, env, 'init', '--quiet');
      await gitWithEnv(directory, env, 'config', 'user.name', 'LFS Cloud Smoke Test');
      await gitWithEnv(directory, env, 'config', 'user.email', `${branch}@example.invalid`);
      await gitWithEnv(directory, env, 'config', 'commit.gpgSign', 'false');
      await gitWithEnv(directory, env, 'lfs', 'install', '--local', '--skip-smudge');
      await gitWithEnv(directory, env, 'remote', 'add', 'origin', remoteUrl);
      await gitWithEnv(directory, env, 'fetch', 'origin');
      await gitWithEnv(directory, env, 'checkout', '--quiet', '-B', branch, 'origin/main');
    };

    await command('git', ['init', '--bare', '--quiet', '--initial-branch=main', central]);
    await configureEnvironment(seedEnv);
    await mkdir(seed);
    await gitWithEnv(seed, seedEnv, 'init', '--quiet', '--initial-branch=main');
    await gitWithEnv(seed, seedEnv, 'config', 'user.name', 'LFS Cloud Smoke Test');
    await gitWithEnv(seed, seedEnv, 'config', 'user.email', 'seed@example.invalid');
    await gitWithEnv(seed, seedEnv, 'config', 'commit.gpgSign', 'false');
    await gitWithEnv(seed, seedEnv, 'lfs', 'install', '--local');
    await gitWithEnv(seed, seedEnv, 'lfs', 'track', 'assets/*.bin');
    await mkdir(join(seed, 'assets'));
    await writeFile(join(seed, 'assets', 'central.bin'), payloadA);
    await gitWithEnv(seed, seedEnv, 'add', '.gitattributes', 'assets/central.bin');
    await gitWithEnv(seed, seedEnv, 'commit', '--quiet', '-m', 'Add central LFS object');
    await gitWithEnv(seed, seedEnv, 'remote', 'add', 'origin', remoteUrl);
    await gitWithEnv(seed, seedEnv, 'push', '--quiet', '--no-verify', 'origin', 'main');

    await configureEnvironment(userBEnv, join(sandbox, 'user-b-credentials'));
    await initializeCheckout(userB, 'private', userBEnv);
    await writeFile(join(userB, 'assets', 'private.bin'), payloadB);
    await gitWithEnv(userB, userBEnv, 'add', 'assets/private.bin');
    await gitWithEnv(userB, userBEnv, 'commit', '--quiet', '-m', 'Add private LFS object');
    await writeLfsMediaObject(userB, oidA, payloadA);

    await configureEnvironment(userAEnv, join(sandbox, 'user-a-credentials'));
    await initializeCheckout(userA, 'main', userAEnv);
    await gitWithEnv(userA, userAEnv, 'config', '--local', 'remote.origin.lfsurl', sourceLfsUrl);
    await command(binaryPath, ['login', '--server', serverBase], {
      cwd: userA,
      env: userAEnv,
      input: 'github_pat_smoke_user_a\n',
    });
    const userAOutput = await command(
      binaryPath,
      ['migrate', '--server', serverBase, '--cache-root', join(sandbox, 'user-a-cache'), '--all-refs'],
      { cwd: userA, env: userAEnv },
    );
    assert(
      userAOutput.includes('target objects: 1 uploaded, 0 already present'),
      'first migration did not upload its object',
    );
    assert(targetObjects.get(oidA)?.equals(payloadA), 'first migration did not pass object bytes through the server');
    assert(
      targetUploads.some(upload => upload.actor === 'user-a' && upload.oid === oidA),
      'user A upload was not authenticated independently',
    );
    assert(
      (await gitWithEnv(userA, userAEnv, 'config', '--file', '.lfsconfig', '--get', 'lfs.url')).trim() === targetLfsUrl,
      'first migration did not commit the LFS Cloud target',
    );
    assert(
      (await gitWithEnv(userA, userAEnv, 'config', '--file', '.lfsconfig', '--get', 'remote.origin.lfsurl')).trim() ===
        sourceLfsUrl,
      'first migration did not commit the legacy source endpoint',
    );
    await gitWithEnv(userA, userAEnv, 'add', '.lfsconfig');
    await gitWithEnv(userA, userAEnv, 'commit', '--quiet', '-m', 'Route LFS through LFS Cloud');
    await gitWithEnv(userA, userAEnv, 'push', '--quiet', '--no-verify', 'origin', 'main');

    await gitWithEnv(userB, userBEnv, 'fetch', 'origin');
    await gitWithEnv(userB, userBEnv, 'cherry-pick', '--quiet', 'origin/main');
    assert(
      (await gitWithEnv(userB, userBEnv, 'config', '--file', '.lfsconfig', '--get', 'remote.origin.lfsurl')).trim() ===
        sourceLfsUrl,
      'follow-up user did not receive the committed legacy source endpoint',
    );
    await rm(lfsMediaPath(userB, oidB), { force: true });
    sourceRequests.clear();
    await command(binaryPath, ['login', '--server', serverBase], {
      cwd: userB,
      env: userBEnv,
      input: 'github_pat_smoke_user_b\n',
    });
    const userBOutput = await command(
      binaryPath,
      ['migrate', '--server', serverBase, '--cache-root', join(sandbox, 'user-b-cache'), '--all-refs'],
      { cwd: userB, env: userBEnv },
    );
    assert(
      userBOutput.includes('target objects: 1 uploaded, 1 already present'),
      'follow-up migration did not reconcile existing and missing target objects',
    );
    assert(targetObjects.get(oidB)?.equals(payloadB), 'follow-up migration did not upload the remaining object');
    assert(
      targetUploads.filter(upload => upload.oid === oidA).length === 1,
      'follow-up migration re-uploaded the central object',
    );
    assert(
      targetUploads.some(upload => upload.actor === 'user-b' && upload.oid === oidB),
      'user B upload did not use user B authentication',
    );
    assert(
      sourceRequests.has(oidB),
      'follow-up migration did not fetch its missing object from the committed legacy URL',
    );
    assert(
      !sourceRequests.has(oidA),
      'follow-up migration fetched an object that was already local and present remotely',
    );
    assert(
      targetBatches.some(batch => batch.actor === 'user-b' && batch.oids.includes(oidA) && batch.oids.includes(oidB)),
      'follow-up migration did not reconcile the complete inventory through the server',
    );
    assert(
      loginUsers.has('user-a') && loginUsers.has('user-b'),
      'both users did not authenticate with independent PATs',
    );
    const lfsEnvironment = await gitWithEnv(userB, userBEnv, 'lfs', 'env');
    assert(lfsEnvironment.includes(`Endpoint=${targetLfsUrl}`), 'legacy source overrode normal LFS Cloud traffic');
    assert(!(await pathExists(join(userA, 'lfscloud.yml'))), 'migration created private server config for user A');
    assert(!(await pathExists(join(userB, 'lfscloud.yml'))), 'migration created private server config for user B');
    assert(serverFailure === undefined, `migration smoke server failed: ${serverFailure ?? ''}`);
  } finally {
    await new Promise<void>(closed => server.close(() => closed()));
  }
}

async function statusSmoke(): Promise<void> {
  const repo = join(sandbox, 'status-repo');
  const cacheRoot = join(sandbox, 'status-cache');
  const configPath = join(sandbox, 'status.yml');
  const gcloudConfigDir = join(sandbox, 'status-gcloud-drive');
  const gitConfig = join(sandbox, 'status-gitconfig');
  const credentialStore = join(sandbox, 'status-credentials');
  await mkdir(repo);
  await mkdir(join(cacheRoot, 'objects'), { recursive: true });
  await mkdir(gcloudConfigDir);
  await writeFile(join(gcloudConfigDir, 'application_default_credentials.json'), '{}');
  await git(repo, 'init', '--quiet');
  await git(repo, 'remote', 'add', 'origin', 'git@github.com:smoke-owner/status.git');

  const server = createNetServer(socket => socket.end());
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
      `server:\n  host: 127.0.0.1\n  port: ${address.port}\n  public_url: ${serverUrl}\n  session_encryption_secret: smoke-status-session-secret-at-least-32-characters\n\nrepository_providers:\n  github-main:\n    type: github\n    api_url: https://api.github.com\n\nstorage_providers:\n  drive-smoke:\n    type: google_drive\n    credentials:\n      type: gcloud\n      config_dir: ${JSON.stringify(gcloudConfigDir)}\n      executable: ${JSON.stringify(process.execPath)}\n    root_folder_id: smoke-root\n\nrepositories:\n  - id: github-main:smoke-owner/status\n    repo_provider: github-main\n    host: github.com\n    owner: smoke-owner\n    name: status\n    provider_repository_id: "8675309"\n    storage_provider: drive-smoke\n`,
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
      env: gitEnv,
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

function hasGitHubPat(): boolean {
  return hasCredential(githubPatEnv);
}

function hasDriveCredential(): boolean {
  const configDir = process.env[driveConfigDirEnv]?.trim();
  return (
    configDir !== undefined &&
    configDir.length > 0 &&
    existsSync(join(configDir, 'application_default_credentials.json'))
  );
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
      name: 'smoke workspace platform defaults',
      run: async () => {
        assert(
          defaultThrowawayRoot('C:\\Users\\smoke', 'win32') === join('C:\\Users\\smoke', 'Projects', 'throwaway'),
          'Windows throwaway root did not use the Projects directory',
        );
        assert(
          defaultThrowawayRoot('/Users/smoke', 'darwin') === join('/Users/smoke', 'Sites', 'throwaway'),
          'macOS throwaway root did not preserve the Sites directory',
        );
        const windowsGcloud = gcloudInvocation(['--version'], 'win32', 'C:\\Windows\\System32\\cmd.exe');
        assert(
          windowsGcloud.executable === 'C:\\Windows\\System32\\cmd.exe' &&
            windowsGcloud.args.join(' ') === '/d /s /c gcloud.cmd --version',
          'Windows gcloud invocation did not use the command launcher',
        );
        const macosGcloud = gcloudInvocation(['--version'], 'darwin');
        assert(
          macosGcloud.executable === 'gcloud' && macosGcloud.args.join(' ') === '--version',
          'macOS gcloud invocation unexpectedly changed',
        );
      },
    },
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
          'config',
          'repository',
          'sessions',
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
    { name: 'default server startup', run: defaultServerStartupSmoke },
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
    { name: 'configuration command workflows', run: configurationCommandsSmoke },
    { name: 'session key rotation safety', run: sessionKeyRotationSafetySmoke },
    {
      name: 'native credential store session key',
      skip: enabledFlag(
        'LFS_CLOUD_RUN_NATIVE_KEYRING_SMOKE',
        'mutates one disposable native credential and removes it afterward',
      ),
      run: () => script('verify-native-session-key-store.sh', 10 * 60 * 1000),
    },
    {
      name: 'default server config path',
      run: () =>
        script('verify-default-config-path.sh', defaultTimeoutMs, {
          LFS_CLOUD_SMOKE_BINARY: binaryPath,
        }),
    },
    { name: 'repository initialization', run: initRepositorySmoke },
    { name: 'historical Git LFS migration planning', run: migrationDryRunSmoke },
    { name: 'Git credential approval', run: () => script('verify-git-credential-approve.sh') },
    {
      name: 'credential-helper fallback',
      run: () => script('verify-git-credential-helper-fallback.sh'),
    },
    { name: 'login workflow', run: () => script('verify-login-command.sh') },
    {
      name: 'per-user GitHub repository authorization',
      run: () => script('verify-github-repository-authorization.sh'),
    },
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
      name: 'server-mediated migration contract',
      run: () => script('verify-migration-history-execution.sh'),
    },
    { name: 'two-user follow-up migration mock contract', run: mockServerMediatedFollowUpMigrationSmoke },
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
      skip: missingCredential(hasGitHubPat, `requires ${githubPatEnv}`),
      run: () =>
        script('verify-github-integration.sh', 30 * 60 * 1000, {
          LFS_CLOUD_RUN_GITHUB_INTEGRATION: '1',
        }),
    },
    {
      name: 'gcloud CLI prerequisite',
      skip: missingCredential(hasDriveCredential, `requires ${driveConfigDirEnv}`),
      run: async () => {
        await gcloud(['--version']);
      },
    },
    {
      name: 'Google Drive disposable folder',
      skip: missingCredential(hasDriveCredential, `requires ${driveConfigDirEnv}`),
      run: () =>
        script('verify-google-drive-integration.sh', 30 * 60 * 1000, {
          LFS_CLOUD_RUN_GOOGLE_DRIVE_INTEGRATION: '1',
        }),
    },
    {
      name: 'black-box Git LFS live transfer',
      skip: missingCredential(
        () => hasGitHubPat() && hasDriveCredential(),
        `requires ${githubPatEnv}, and ${driveConfigDirEnv}`,
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
  await Promise.all([...backgroundCommands].map(command => command.stop()));
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

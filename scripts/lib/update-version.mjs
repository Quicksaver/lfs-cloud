#!/usr/bin/env node

import { readFileSync, renameSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import process from 'node:process';

function fail(message) {
  process.stderr.write(`error: ${message}\n`);
  process.exit(1);
}

function replacePackageVersion(content, oldVersion, newVersion, fileName) {
  const lines = content.split('\n');
  let inPackage = false;
  let replacements = 0;

  const updated = lines.map(line => {
    if (line === '[package]') {
      inPackage = true;
      return line;
    }
    if (inPackage && line.startsWith('[')) inPackage = false;
    if (!inPackage) return line;

    const match = line.match(/^version = "([^"]+)"$/);
    if (match === null) return line;
    if (match[1] !== oldVersion) {
      fail(`${fileName} has version ${match[1]}, expected ${oldVersion}`);
    }
    replacements += 1;
    return `version = "${newVersion}"`;
  });

  if (replacements !== 1) fail(`${fileName} must contain exactly one [package] version`);
  return updated.join('\n');
}

function replaceLockVersion(content, oldVersion, newVersion) {
  const blocks = content.split(/(?=^\[\[package\]\]$)/m);
  let replacements = 0;

  const updated = blocks.map(block => {
    if (!/^name = "lfscloud"$/m.test(block)) return block;

    const match = block.match(/^version = "([^"]+)"$/m);
    if (match === null) fail('Cargo.lock lfscloud entry has no version');
    if (match[1] !== oldVersion) {
      fail(`Cargo.lock has version ${match[1]}, expected ${oldVersion}`);
    }
    replacements += 1;
    return block.replace(/^version = "[^"]+"$/m, `version = "${newVersion}"`);
  });

  if (replacements !== 1) fail('Cargo.lock must contain exactly one lfscloud package entry');
  return updated.join('');
}

function replaceJsonVersion(content, oldVersion, newVersion) {
  const packageJson = JSON.parse(content);
  if (packageJson.version !== oldVersion) {
    fail(`package.json has version ${String(packageJson.version)}, expected ${oldVersion}`);
  }
  packageJson.version = newVersion;
  return `${JSON.stringify(packageJson, null, 2)}\n`;
}

function writeAtomically(path, content) {
  const temporary = join(dirname(path), `.${process.pid}.${Date.now()}.tmp`);
  const mode = statSync(path).mode & 0o777;
  writeFileSync(temporary, content, { encoding: 'utf8', mode });
  renameSync(temporary, path);
}

const [repoRoot, oldVersion, newVersion] = process.argv.slice(2);
const semver = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;

if (repoRoot === undefined || oldVersion === undefined || newVersion === undefined) {
  fail('usage: update-version.mjs REPO_ROOT OLD_VERSION NEW_VERSION');
}
if (!semver.test(oldVersion) || !semver.test(newVersion)) {
  fail('old and new versions must be plain semantic versions');
}
if (oldVersion === newVersion) fail('new version must differ from old version');

const cargoTomlPath = join(repoRoot, 'Cargo.toml');
const cargoLockPath = join(repoRoot, 'Cargo.lock');
const packageJsonPath = join(repoRoot, 'package.json');

const cargoToml = replacePackageVersion(readFileSync(cargoTomlPath, 'utf8'), oldVersion, newVersion, 'Cargo.toml');
const cargoLock = replaceLockVersion(readFileSync(cargoLockPath, 'utf8'), oldVersion, newVersion);
const packageJson = replaceJsonVersion(readFileSync(packageJsonPath, 'utf8'), oldVersion, newVersion);

writeAtomically(cargoTomlPath, cargoToml);
writeAtomically(cargoLockPath, cargoLock);
writeAtomically(packageJsonPath, packageJson);

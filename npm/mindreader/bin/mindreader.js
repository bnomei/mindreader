#!/usr/bin/env node
"use strict";

const childProcess = require("child_process");
const crypto = require("crypto");
const fs = require("fs");
const https = require("https");
const os = require("os");
const path = require("path");

const packageJson = require("../package.json");
const BIN = "mindreader";

if (require.main === module) {
  main().catch((error) => {
    console.error(`mindreader npm wrapper: ${error.message}`);
    process.exit(1);
  });
}

async function main() {
  const repository = process.env.MINDREADER_REPOSITORY || "bnomei/mindreader";
  const version = normalizeVersion(process.env.MINDREADER_VERSION || packageJson.version);
  const release = releaseTarget();
  const binaryPath = path.join(cacheDir(), version, release.target, release.binary);

  if (!fs.existsSync(binaryPath)) {
    await installRelease(binaryPath, release, repository, version);
  }

  const result = childProcess.spawnSync(binaryPath, process.argv.slice(2), { stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.signal) {
    process.kill(process.pid, result.signal);
    return;
  }
  process.exit(result.status ?? 1);
}

function normalizeVersion(version) {
  const normalized = version.startsWith("v") ? version : `v${version}`;
  if (!/^v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(normalized)) {
    throw new Error(`invalid release version ${version}`);
  }
  return normalized;
}

function releaseTarget(platform = process.platform, arch = process.arch) {
  if (platform === "linux" && arch === "x64") return unixRelease("x86_64-unknown-linux-gnu");
  if (platform === "linux" && arch === "arm64") return unixRelease("aarch64-unknown-linux-gnu");
  if (platform === "darwin" && arch === "x64") return unixRelease("x86_64-apple-darwin");
  if (platform === "darwin" && arch === "arm64") return unixRelease("aarch64-apple-darwin");
  if (platform === "win32" && arch === "x64") {
    return { target: "x86_64-pc-windows-msvc", archiveExt: ".zip", binary: `${BIN}.exe` };
  }
  throw new Error(`unsupported platform ${platform}/${arch}`);
}

function unixRelease(target) {
  return { target, archiveExt: ".tar.gz", binary: BIN };
}

function cacheDir() {
  if (process.env.MINDREADER_NPM_CACHE) return process.env.MINDREADER_NPM_CACHE;
  if (process.platform === "win32") {
    return path.join(process.env.LOCALAPPDATA || os.tmpdir(), "mindreader", "npm");
  }
  return path.join(process.env.XDG_CACHE_HOME || path.join(os.homedir(), ".cache"), "mindreader", "npm");
}

async function installRelease(binaryPath, release, repository, version) {
  const archive = `${BIN}-${version}-${release.target}${release.archiveExt}`;
  const baseUrl = `https://github.com/${repository}/releases/download/${version}/${archive}`;
  const temporaryDirectory = fs.mkdtempSync(path.join(os.tmpdir(), "mindreader-npm-"));

  try {
    const archivePath = path.join(temporaryDirectory, archive);
    const checksumPath = `${archivePath}.sha256`;
    await download(`${baseUrl}.sha256`, checksumPath);
    await download(baseUrl, archivePath);
    verifyChecksum(archivePath, checksumPath);
    extractArchive(archivePath, temporaryDirectory, release.archiveExt);

    const extracted = path.join(temporaryDirectory, release.binary);
    if (!fs.existsSync(extracted)) throw new Error(`release archive did not contain ${release.binary}`);
    fs.mkdirSync(path.dirname(binaryPath), { recursive: true });
    fs.copyFileSync(extracted, binaryPath);
    if (process.platform !== "win32") fs.chmodSync(binaryPath, 0o755);
  } finally {
    fs.rmSync(temporaryDirectory, { force: true, recursive: true });
  }
}

function download(url, destination, redirects = 0) {
  return new Promise((resolve, reject) => {
    const request = https.get(url, { headers: { "user-agent": "mindreader-npm-wrapper" } }, (response) => {
      if (response.statusCode >= 300 && response.statusCode < 400 && response.headers.location) {
        response.resume();
        if (redirects >= 5) return reject(new Error(`too many redirects downloading ${url}`));
        return download(response.headers.location, destination, redirects + 1).then(resolve, reject);
      }
      if (response.statusCode !== 200) {
        response.resume();
        return reject(new Error(`download failed with HTTP ${response.statusCode}: ${url}`));
      }
      const file = fs.createWriteStream(destination);
      response.pipe(file);
      file.on("finish", () => file.close(resolve));
      file.on("error", reject);
    });
    request.on("error", reject);
  });
}

function verifyChecksum(archivePath, checksumPath) {
  const expected = fs.readFileSync(checksumPath, "utf8").trim().split(/\s+/)[0].toLowerCase();
  if (!/^[a-f0-9]{64}$/.test(expected)) throw new Error("checksum file did not contain a SHA-256 digest");
  const actual = crypto.createHash("sha256").update(fs.readFileSync(archivePath)).digest("hex");
  if (actual !== expected) throw new Error("checksum mismatch");
}

function extractArchive(archivePath, destination, archiveExt) {
  if (archiveExt === ".tar.gz") {
    childProcess.execFileSync("tar", ["-xzf", archivePath, "-C", destination], { stdio: "ignore" });
    return;
  }
  const powershell = path.join(
    process.env.SystemRoot || "C:\\Windows",
    "System32", "WindowsPowerShell", "v1.0", "powershell.exe",
  );
  childProcess.execFileSync(
    powershell,
    ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command",
      "Expand-Archive -LiteralPath $args[0] -DestinationPath $args[1] -Force", archivePath, destination],
    { stdio: "ignore" },
  );
}

module.exports = { cacheDir, normalizeVersion, releaseTarget, verifyChecksum };

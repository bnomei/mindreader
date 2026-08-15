"use strict";

const assert = require("node:assert/strict");
const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");

const { normalizeVersion, releaseTarget, verifyChecksum } = require("../bin/mindreader.js");

test("normalizes release versions", () => {
  assert.equal(normalizeVersion("0.1.0"), "v0.1.0");
  assert.equal(normalizeVersion("v0.1.0"), "v0.1.0");
  assert.throws(() => normalizeVersion("../../latest"), /invalid release version/);
});

test("maps supported Node hosts to release assets", () => {
  assert.equal(releaseTarget("linux", "x64").target, "x86_64-unknown-linux-gnu");
  assert.equal(releaseTarget("linux", "arm64").target, "aarch64-unknown-linux-gnu");
  assert.equal(releaseTarget("darwin", "x64").target, "x86_64-apple-darwin");
  assert.equal(releaseTarget("darwin", "arm64").target, "aarch64-apple-darwin");
  assert.equal(releaseTarget("win32", "x64").target, "x86_64-pc-windows-msvc");
  assert.throws(() => releaseTarget("freebsd", "x64"), /unsupported platform/);
});

test("accepts matching SHA-256 and rejects mismatches", () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), "mindreader-wrapper-test-"));
  try {
    const archive = path.join(directory, "archive");
    const checksum = `${archive}.sha256`;
    fs.writeFileSync(archive, "mindreader fixture");
    const digest = crypto.createHash("sha256").update(fs.readFileSync(archive)).digest("hex");
    fs.writeFileSync(checksum, `${digest}  archive\n`);
    assert.doesNotThrow(() => verifyChecksum(archive, checksum));
    fs.writeFileSync(checksum, `${"0".repeat(64)}  archive\n`);
    assert.throws(() => verifyChecksum(archive, checksum), /checksum mismatch/);
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
});

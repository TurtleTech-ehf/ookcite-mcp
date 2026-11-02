const test = require("node:test");
const assert = require("node:assert");
const crypto = require("node:crypto");
const path = require("node:path");
const { execFileSync } = require("node:child_process");

const fs = require("node:fs");
const os = require("node:os");

const {
  redirectTarget,
  verifyChecksum,
  tryBinstall,
  tryCargoInstall,
  MAX_REDIRECTS,
} = require("../scripts/install.js");

const RELEASE = "https://github.com/TurtleTech-ehf/ookcite-mcp/releases/download/v0.6.5/a.tar.gz";

test("a redirect is followed only while it stays on https", () => {
  assert.strictEqual(
    redirectTarget(RELEASE, "https://objects.githubusercontent.com/a.tar.gz", MAX_REDIRECTS),
    "https://objects.githubusercontent.com/a.tar.gz"
  );
  // The binary is written to disk and made executable, so a hop that leaves
  // TLS hands the contents of that file to anyone on the path.
  assert.throws(
    () => redirectTarget(RELEASE, "http://objects.githubusercontent.com/a.tar.gz", MAX_REDIRECTS),
    /https/
  );
  // A protocol-relative hop inherits https from the base, which is what the
  // real release redirect to objects.githubusercontent.com looks like.
  assert.strictEqual(
    redirectTarget(RELEASE, "//objects.githubusercontent.com/a.tar.gz", MAX_REDIRECTS),
    "https://objects.githubusercontent.com/a.tar.gz"
  );
});

test("a redirect chain is bounded", () => {
  assert.throws(() => redirectTarget(RELEASE, RELEASE, 0), /redirect/i);
});

test("an archive is installed only against a published checksum", () => {
  const archive = Buffer.from("release archive");
  const digest = crypto.createHash("sha256").update(archive).digest("hex");

  assert.strictEqual(verifyChecksum("a.tar.gz", archive, Buffer.from(`${digest}  a.tar.gz\n`)), digest);
  assert.strictEqual(
    verifyChecksum("a.zip", archive, Buffer.from(`${digest.toUpperCase()}  a.zip\r\n`)),
    digest
  );

  // A checksum that could not be fetched is not a reason to install the
  // binary anyway: dropping one request would then be enough to defeat the
  // check entirely.
  assert.throws(() => verifyChecksum("a.tar.gz", archive, null), /unverified/i);
  assert.throws(() => verifyChecksum("a.tar.gz", archive, Buffer.from("")), /unverified/i);
  assert.throws(() => verifyChecksum("a.tar.gz", archive, Buffer.from("not-a-digest  a")), /unverified/i);
  assert.throws(
    () => verifyChecksum("a.tar.gz", archive, Buffer.from(`${"0".repeat(64)}  a.tar.gz`)),
    /mismatch/i
  );
});

test("requiring the installer does not run it", () => {
  // Without a main guard, merely loading this module downloads and chmods a
  // binary, so the tests above would install whatever the network returned.
  const child = execFileSync(process.execPath, ["-e", "require('./scripts/install.js')"], {
    cwd: path.join(__dirname, ".."),
    encoding: "utf8",
  });
  assert.strictEqual(child.trim(), "");
});

test("the cargo fallbacks never hand the install path to a shell", { skip: process.platform === "win32" }, async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "ookcite-install-"));
  const fakeBin = path.join(dir, "bin");
  fs.mkdirSync(fakeBin);
  const argvLog = path.join(dir, "argv");
  fs.writeFileSync(
    path.join(fakeBin, "cargo"),
    `#!/bin/sh\nprintf '%s\\n' "$@" >> ${JSON.stringify(argvLog)}\n`
  );
  fs.chmodSync(path.join(fakeBin, "cargo"), 0o755);

  // npm puts the package wherever the user's directory layout says, and that
  // path is not ours to trust once it reaches a command line.
  const marker = path.join(dir, "executed");
  const target = path.join(dir, `pkg$(touch ${marker})`);

  const savedPath = process.env.PATH;
  process.env.PATH = `${fakeBin}${path.delimiter}${savedPath}`;
  try {
    assert.strictEqual(await tryBinstall(target), true);
    assert.strictEqual(await tryCargoInstall(target), true);
  } finally {
    process.env.PATH = savedPath;
  }

  assert.ok(!fs.existsSync(marker), "the install path was evaluated by a shell");
  const argv = fs.readFileSync(argvLog, "utf8").split("\n");
  assert.ok(argv.includes(target), "the install path did not survive as one argument");
});

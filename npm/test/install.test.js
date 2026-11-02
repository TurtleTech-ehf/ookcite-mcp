const test = require("node:test");
const assert = require("node:assert");
const crypto = require("node:crypto");
const path = require("node:path");
const { execFileSync } = require("node:child_process");

const fs = require("node:fs");
const os = require("node:os");
const https = require("node:https");
const { EventEmitter } = require("node:events");

const {
  redirectTarget,
  verifyChecksum,
  downloadAndVerify,
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
  assert.throws(() => redirectTarget(RELEASE, RELEASE, 0), /Too many redirects/);
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

// Answers every https.get from `respond(url)` without opening a socket, and
// records the URLs asked for. `respond` returns [status, headers, body], or
// "error" for a request that fails outright.
async function withStubbedGet(respond, run) {
  const requested = [];
  const realGet = https.get;
  https.get = (url, options, onResponse) => {
    url = String(url);
    requested.push(url);
    const request = new EventEmitter();
    setImmediate(() => {
      const answer = respond(url);
      if (answer === "error") {
        request.emit("error", new Error("connection reset"));
        return;
      }
      const [status, headers, body] = answer;
      const response = new EventEmitter();
      response.statusCode = status;
      response.headers = headers;
      onResponse(response);
      setImmediate(() => {
        if (body.length) response.emit("data", Buffer.from(body));
        response.emit("end");
      });
    });
    return request;
  };
  try {
    return { result: await run(), requested };
  } catch (error) {
    return { error, requested };
  } finally {
    https.get = realGet;
  }
}

const ASSET = "ookcite-mcp-x86_64-unknown-linux-gnu.tar.gz";
const ARCHIVE = Buffer.from("release archive");
const DIGEST = crypto.createHash("sha256").update(ARCHIVE).digest("hex");

test("the download path refuses an archive whose checksum did not arrive", async () => {
  // verifyChecksum alone cannot show this: the caller deciding to skip it
  // when the .sha256 request fails is the bug this path used to have.
  for (const sum of [[404, {}, ""], "error"]) {
    const { error } = await withStubbedGet(
      (url) => (url.endsWith(".sha256") ? sum : [200, {}, ARCHIVE]),
      () => downloadAndVerify(ASSET)
    );
    assert.match(String(error), /unverified/);
  }

  const { result } = await withStubbedGet(
    (url) => (url.endsWith(".sha256") ? [200, {}, `${DIGEST}  ${ASSET}\n`] : [200, {}, ARCHIVE]),
    () => downloadAndVerify(ASSET)
  );
  assert.deepStrictEqual(result, ARCHIVE);
});

test("the download path never makes a request over plaintext http", async () => {
  const { error, requested } = await withStubbedGet(
    (url) =>
      url.startsWith("https://github.com/")
        ? [302, { location: url.replace("https://github.com/", "http://github.com/") }, ""]
        : [200, {}, url.endsWith(".sha256") ? `${DIGEST}  ${ASSET}\n` : ARCHIVE],
    () => downloadAndVerify(ASSET)
  );
  assert.match(String(error), /non-https/);
  assert.deepStrictEqual(requested.filter((url) => !url.startsWith("https:")), []);
});

test("the download path gives up on a redirect loop", { timeout: 5000 }, async () => {
  const { error, requested } = await withStubbedGet(
    (url) => [302, { location: url }, ""],
    () => downloadAndVerify(ASSET)
  );
  assert.match(String(error), /Too many redirects/);
  // The archive and its checksum each get the first request plus the bound.
  assert.ok(requested.length <= 2 * (MAX_REDIRECTS + 1), `${requested.length} requests`);
});

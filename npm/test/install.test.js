const test = require("node:test");
const assert = require("node:assert");
const crypto = require("node:crypto");
const path = require("node:path");
const { execFileSync } = require("node:child_process");

const { redirectTarget, verifyChecksum, MAX_REDIRECTS } = require("../scripts/install.js");

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

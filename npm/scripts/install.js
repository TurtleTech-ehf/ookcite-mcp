const https = require("https");
const fs = require("fs");
const path = require("path");
const { execFileSync } = require("child_process");
const crypto = require("crypto");
const zlib = require("zlib");

const VERSION = require("../package.json").version;
const REPO = "TurtleTech-ehf/ookcite-mcp";
const BIN_DIR = path.join(__dirname, "..", "bin");

const PLATFORM_MAP = {
  "darwin-x64": "ookcite-mcp-x86_64-apple-darwin",
  "darwin-arm64": "ookcite-mcp-aarch64-apple-darwin",
  "linux-x64": "ookcite-mcp-x86_64-unknown-linux-gnu",
  "linux-arm64": "ookcite-mcp-aarch64-unknown-linux-gnu",
  "win32-x64": "ookcite-mcp-x86_64-pc-windows-msvc",
};

function getPlatformKey() {
  return `${process.platform}-${process.arch}`;
}

function getAssetName() {
  const key = getPlatformKey();
  const name = PLATFORM_MAP[key];
  if (!name) {
    throw new Error(
      `Unsupported platform: ${key}. Supported: ${Object.keys(PLATFORM_MAP).join(", ")}`
    );
  }
  const ext = process.platform === "win32" ? ".zip" : ".tar.gz";
  return `${name}${ext}`;
}

const MAX_REDIRECTS = 5;

// The archive lands on disk as an executable, so every hop has to stay under
// TLS: one plaintext hop lets anyone on the path choose what gets run.
function redirectTarget(currentUrl, location, redirectsLeft) {
  if (redirectsLeft <= 0) {
    throw new Error(`Too many redirects for ${currentUrl}`);
  }
  const next = new URL(location, currentUrl);
  if (next.protocol !== "https:") {
    throw new Error(`Refusing a redirect from ${currentUrl} to non-https ${next.protocol}//${next.host}`);
  }
  return next.toString();
}

function fetch(urlStr, redirectsLeft = MAX_REDIRECTS) {
  return new Promise((resolve, reject) => {
    let url;
    try {
      url = redirectTarget(urlStr, urlStr, MAX_REDIRECTS);
    } catch (e) {
      return reject(e);
    }
    https.get(url, { headers: { "User-Agent": "ookcite-mcp-npm" } }, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        let next;
        try {
          next = redirectTarget(url, res.headers.location, redirectsLeft);
        } catch (e) {
          return reject(e);
        }
        return fetch(next, redirectsLeft - 1).then(resolve, reject);
      }
      if (res.statusCode !== 200) {
        return reject(new Error(`HTTP ${res.statusCode} for ${url}`));
      }
      const chunks = [];
      res.on("data", (c) => chunks.push(c));
      res.on("end", () => resolve(Buffer.concat(chunks)));
      res.on("error", reject);
    }).on("error", reject);
  });
}

// A checksum that did not arrive is not a checksum that passed. Treating it
// as one means failing a single request is enough to skip verification.
function verifyChecksum(asset, archive, checksumFile) {
  const published = checksumFile
    ? checksumFile.toString("utf8").trim().split(/\s+/)[0].toLowerCase()
    : "";
  if (!/^[0-9a-f]{64}$/.test(published)) {
    throw new Error(
      `No readable SHA-256 published for ${asset}; refusing to install an unverified binary.`
    );
  }
  const actual = crypto.createHash("sha256").update(archive).digest("hex");
  if (actual !== published) {
    throw new Error(`Checksum mismatch for ${asset}: expected ${published}, got ${actual}`);
  }
  return published;
}

async function downloadAndVerify(asset) {
  const baseUrl = `https://github.com/${REPO}/releases/download/v${VERSION}`;
  const archiveUrl = `${baseUrl}/${asset}`;
  const checksumUrl = `${archiveUrl}.sha256`;

  console.log(`Downloading ${asset}...`);
  const [archive, checksumFile] = await Promise.all([
    fetch(archiveUrl),
    fetch(checksumUrl).catch(() => null),
  ]);

  verifyChecksum(asset, archive, checksumFile);
  console.log("Checksum verified.");

  return archive;
}

function extractTarGz(buffer) {
  const decompressed = zlib.gunzipSync(buffer);
  // Minimal tar extraction: find the binary file in the archive
  let offset = 0;
  while (offset < decompressed.length) {
    const header = decompressed.subarray(offset, offset + 512);
    if (header.every((b) => b === 0)) break;

    const name = header.subarray(0, 100).toString("utf8").replace(/\0/g, "").trim();
    const sizeOctal = header.subarray(124, 136).toString("utf8").replace(/\0/g, "").trim();
    const size = parseInt(sizeOctal, 8) || 0;
    const dataStart = offset + 512;
    const dataEnd = dataStart + size;

    if (name === "ookcite-mcp" || name.endsWith("/ookcite-mcp")) {
      const data = decompressed.subarray(dataStart, dataEnd);
      const outPath = path.join(BIN_DIR, "ookcite-mcp");
      fs.writeFileSync(outPath, data);
      fs.chmodSync(outPath, 0o755);
      return true;
    }

    // Advance to next 512-byte boundary
    offset = dataStart + Math.ceil(size / 512) * 512;
  }
  return false;
}

function extractZip(buffer) {
  // Minimal zip extraction: find end-of-central-directory, then local file headers
  // For simplicity, search for the binary name in local file headers
  const binaryName = "ookcite-mcp.exe";
  for (let i = 0; i < buffer.length - 30; i++) {
    // Local file header signature: PK\x03\x04
    if (buffer[i] === 0x50 && buffer[i + 1] === 0x4b && buffer[i + 2] === 0x03 && buffer[i + 3] === 0x04) {
      const nameLen = buffer.readUInt16LE(i + 26);
      const extraLen = buffer.readUInt16LE(i + 28);
      const compSize = buffer.readUInt32LE(i + 18);
      const name = buffer.subarray(i + 30, i + 30 + nameLen).toString("utf8");

      if (name === binaryName || name.endsWith("/" + binaryName)) {
        const compressionMethod = buffer.readUInt16LE(i + 8);
        const dataStart = i + 30 + nameLen + extraLen;

        let data;
        if (compressionMethod === 0) {
          // Stored (no compression)
          data = buffer.subarray(dataStart, dataStart + compSize);
        } else if (compressionMethod === 8) {
          // Deflated
          data = zlib.inflateRawSync(buffer.subarray(dataStart, dataStart + compSize));
        } else {
          continue;
        }

        const outPath = path.join(BIN_DIR, binaryName);
        fs.writeFileSync(outPath, data);
        return true;
      }
    }
  }
  return false;
}

// The install path comes from wherever npm placed the package, so it must
// reach cargo as one argument rather than as text a shell gets to read.
function tryBinstall(binDir = BIN_DIR) {
  try {
    execFileSync("cargo", ["binstall", "ookcite-mcp", "--no-confirm", "--install-path", binDir], {
      stdio: "inherit",
    });
    return true;
  } catch {
    return false;
  }
}

function tryCargoInstall(binDir = BIN_DIR) {
  try {
    execFileSync("cargo", ["install", "ookcite-mcp", "--root", path.join(binDir, "..")], {
      stdio: "inherit",
    });
    return true;
  } catch {
    return false;
  }
}

async function main() {
  const ext = process.platform === "win32" ? ".exe" : "";
  const binaryPath = path.join(BIN_DIR, `ookcite-mcp${ext}`);

  // Skip if binary already exists (e.g. local development)
  if (fs.existsSync(binaryPath)) {
    console.log("ookcite-mcp binary already present, skipping download.");
    return;
  }

  fs.mkdirSync(BIN_DIR, { recursive: true });

  // Try GitHub release download first
  try {
    const asset = getAssetName();
    const archive = await downloadAndVerify(asset);

    let extracted;
    if (asset.endsWith(".tar.gz")) {
      extracted = extractTarGz(archive);
    } else {
      extracted = extractZip(archive);
    }

    if (extracted) {
      console.log("ookcite-mcp installed successfully.");
      return;
    }
    console.warn("Could not extract binary from archive.");
  } catch (e) {
    console.warn(`Download failed: ${e.message}`);
  }

  // Fallback: cargo binstall
  console.log("Trying cargo binstall...");
  if (tryBinstall()) {
    console.log("ookcite-mcp installed via cargo binstall.");
    return;
  }

  // Fallback: cargo install
  console.log("Trying cargo install (this may take a few minutes)...");
  if (tryCargoInstall()) {
    console.log("ookcite-mcp installed via cargo install.");
    return;
  }

  console.error(
    "\nFailed to install ookcite-mcp. Install manually:\n" +
    "  cargo binstall ookcite-mcp\n" +
    "  # or download from https://github.com/TurtleTech-ehf/ookcite-mcp/releases"
  );
  process.exit(1);
}

if (require.main === module) {
  main();
}

module.exports = { redirectTarget, verifyChecksum, tryBinstall, tryCargoInstall, MAX_REDIRECTS };

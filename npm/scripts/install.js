const https = require("https");
const http = require("http");
const fs = require("fs");
const path = require("path");
const { execSync } = require("child_process");
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

function fetch(urlStr) {
  return new Promise((resolve, reject) => {
    const get = urlStr.startsWith("https:") ? https.get : http.get;
    get(urlStr, { headers: { "User-Agent": "ookcite-mcp-npm" } }, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        return fetch(res.headers.location).then(resolve, reject);
      }
      if (res.statusCode !== 200) {
        return reject(new Error(`HTTP ${res.statusCode} for ${urlStr}`));
      }
      const chunks = [];
      res.on("data", (c) => chunks.push(c));
      res.on("end", () => resolve(Buffer.concat(chunks)));
      res.on("error", reject);
    }).on("error", reject);
  });
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

  if (checksumFile) {
    const expected = checksumFile.toString("utf8").trim().split(/\s+/)[0].toLowerCase();
    const actual = crypto.createHash("sha256").update(archive).digest("hex");
    if (actual !== expected) {
      throw new Error(`Checksum mismatch: expected ${expected}, got ${actual}`);
    }
    console.log("Checksum verified.");
  }

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

async function tryBinstall() {
  try {
    execSync("cargo binstall ookcite-mcp --no-confirm --install-path " + JSON.stringify(BIN_DIR), {
      stdio: "inherit",
    });
    return true;
  } catch {
    return false;
  }
}

async function tryCargoInstall() {
  try {
    execSync("cargo install ookcite-mcp --root " + JSON.stringify(path.join(BIN_DIR, "..")), {
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
  if (await tryBinstall()) {
    console.log("ookcite-mcp installed via cargo binstall.");
    return;
  }

  // Fallback: cargo install
  console.log("Trying cargo install (this may take a few minutes)...");
  if (await tryCargoInstall()) {
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

main();

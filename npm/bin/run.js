#!/usr/bin/env node

const { execFileSync } = require("child_process");
const path = require("path");
const fs = require("fs");

const ext = process.platform === "win32" ? ".exe" : "";
const binary = path.join(__dirname, `ookcite-mcp${ext}`);

if (!fs.existsSync(binary)) {
  console.error(
    "ookcite-mcp binary not found. Try reinstalling: npm install @turtletech/ookcite-mcp"
  );
  process.exit(1);
}

try {
  execFileSync(binary, process.argv.slice(2), { stdio: "inherit" });
} catch (e) {
  if (e.status !== null) {
    process.exit(e.status);
  }
  throw e;
}

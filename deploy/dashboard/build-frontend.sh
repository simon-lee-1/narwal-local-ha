#!/bin/sh
# Rebuild the included upstream card patch in a new temporary checkout.
set -eu
umask 077

if [ "$#" -ne 1 ]; then
  echo "Usage: sh build-frontend.sh NEW-OUTPUT.js" >&2
  exit 2
fi

output=$1
case "$output" in
  /*) ;;
  *) output="$(pwd)/$output" ;;
esac
if [ -e "$output" ]; then
  echo "Output already exists; refusing to overwrite it." >&2
  exit 2
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
build_dir=$(mktemp -d "${TMPDIR:-/tmp}/narwal-card.XXXXXXXX")
trap 'rm -rf -- "$build_dir"' EXIT HUP INT TERM
revision=045c3ac946d4ef0a01201f306e9949643984a155
timeout 120s git clone --quiet https://github.com/PiotrMachowski/Lovelace-Xiaomi-Vacuum-Map-card.git "$build_dir/source"
cd "$build_dir/source"
git checkout --quiet --detach "$revision"
test "$(git rev-parse HEAD)" = "$revision"
git apply --check "$script_dir/xiaomi-room-source.patch"
git apply "$script_dir/xiaomi-room-source.patch"
timeout 300s npm ci --ignore-scripts --no-audit --no-fund

# The pinned build tool uses tslib's legacy folder export. Modern Node
# requires an equivalent wildcard export; emitted library code is unchanged.
node - <<'JS'
const fs = require("fs");
const path = "node_modules/rollup-plugin-typescript2/node_modules/tslib/package.json";
const pkg = JSON.parse(fs.readFileSync(path, "utf8"));
if (pkg.version !== "2.1.0" || pkg.exports["./"] !== "./") {
  throw new Error("Unexpected locked tslib build dependency");
}
pkg.exports["./*"] = "./*";
fs.writeFileSync(path, JSON.stringify(pkg, null, 2) + "\n");
JS
timeout 180s node node_modules/rollup/dist/bin/rollup -c
node - "$output" <<'JS'
const fs = require("fs");
const source = fs.readFileSync("dist/xiaomi-vacuum-map-card.js", "utf8");
const placeholder = "@VACUUM_MAP_CARD_VERSION_PLACEHOLDER@";
if (source.split(placeholder).length !== 2 || source.includes("sourceMappingURL")) {
  throw new Error("Unexpected frontend build output");
}
const header = "/*! Xiaomi Vacuum Map Card, Copyright (c) 2021 Piotr Machowski, MIT.\n" +
  " * Modified for dynamic room catalogs; upstream 045c3ac946d4ef0a01201f306e9949643984a155.\n" +
  " * Source patch/build instructions and dependency license texts accompany this resource.\n */\n";
fs.writeFileSync(process.argv[2], header + source.replace(placeholder, "narwal-room-source.3"),
  {flag: "wx", mode: 0o600});
JS
sha256sum "$output"

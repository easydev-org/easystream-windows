#!/usr/bin/env bash
# Fetch the shared web UI into rust/webui/ so `include_str!` can embed it.
#
# The UI lives in exactly ONE place (easydev-org/easystream-webui); every daemon
# embeds it at build time instead of vendoring a copy.
set -Eeuo pipefail
cd "$(dirname "$0")"

REF="${WEBUI_REF:-main}"
PROXY="${HTTPS_PROXY:-${HTTP_PROXY:-}}"
DEST="rust/webui"
URL="https://codeload.github.com/easydev-org/easystream-webui/tar.gz/refs/heads/${REF}"

rm -rf "${DEST}"; mkdir -p "${DEST}"
tmp="$(mktemp -d)"; trap 'rm -rf "${tmp}"' EXIT
curl -fsSL ${PROXY:+-x "${PROXY}"} -o "${tmp}/webui.tgz" "${URL}"
tar xzf "${tmp}/webui.tgz" -C "${tmp}"
cp "${tmp}"/easystream-webui-*/webui/app.js     "${DEST}/app.js"
cp "${tmp}"/easystream-webui-*/webui/index.html "${DEST}/index.html"
echo "webui ${REF} -> ${DEST} ($(wc -c <"${DEST}/app.js") bytes app.js)"

#!/usr/bin/env bash
# Runs one cargo command for the desktop crate inside the Linux check image, on a copy of this
# working tree, with the same cache volumes as scripts/package/linux-cargo-check.mjs (run that once
# first: it builds the image). For machines without a Rust toolchain or the WebKitGTK headers.
# Usage: scripts/linux/cargo-in-docker.sh <cargo args...>   e.g. `test --lib computer::linux`
set -euo pipefail

ARCH="${GENTLE_DOT_ARCH:-amd64}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

exec docker run --rm --platform "linux/${ARCH}" \
	-v "${REPO}:/src:ro" \
	-v "gentle-dot-linux-${ARCH}-cargo:/usr/local/cargo/registry" \
	-v "gentle-dot-linux-${ARCH}-check-target:/work/apps/desktop/src-tauri/target" \
	"gentle-dot-linux-check:${ARCH}" \
	bash -euc '
		set -o pipefail
		cd /src
		git ls-files -z --cached --others --exclude-standard -- . ":!:.codegraph" ":!:.atl" |
			tar --null --files-from=- --ignore-failed-read -cf - 2>/dev/null | tar -xf - -C /work
		cd /work
		mkdir -p packages/ui/dist/app
		[ -e packages/ui/dist/app/index.html ] || printf "<!doctype html>\n" >packages/ui/dist/app/index.html
		cd /work/apps/desktop/src-tauri
		cargo "$@"
	' cargo-in-docker "$@"

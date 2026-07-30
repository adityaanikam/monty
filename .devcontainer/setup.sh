#!/usr/bin/env bash
# Provision a devcontainer/Codespace for monty. Run once by postCreateCommand;
# safe to re-run by hand.
#
# The one non-obvious step is Python: `monty-datatest` and `monty-bench` enable
# pyo3's `auto-initialize` to embed CPython, which requires a *shared*
# libpython. The stock devcontainer images ship a static one, so pyo3's build
# script fails outright. uv's managed CPython is built with --enable-shared, so
# installing the repo's pinned interpreter through uv is also what fixes pyo3.
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v uv >/dev/null; then
    curl -LsSf https://astral.sh/uv/install.sh | sh
fi
export PATH="$HOME/.local/bin:$PATH"

# Creates .venv from the version in .python-version, downloading it if needed.
make install-py
make install-js

# pyo3 links libpython dynamically but emits no rpath, so the loader needs
# LIBDIR at run time or the datatest/bench binaries die on startup.
libdir="$(.venv/bin/python -c 'import sysconfig; print(sysconfig.get_config_var("LIBDIR"))')"

# Guarded so a container rebuild doesn't append a second copy.
if ! grep -q 'monty devcontainer' "$HOME/.bashrc" 2>/dev/null; then
    cat >>"$HOME/.bashrc" <<EOF

# monty devcontainer
export PATH="\$HOME/.local/bin:\$PATH"
export LD_LIBRARY_PATH="$libdir\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
EOF
fi

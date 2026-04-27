#  Copyright (c) Microsoft Corporation. All rights reserved.
#  Licensed under the MIT License. See License.txt in the project root for license information.

# Devcontainer for Devcontainers.app (Tauri 2 + Deno).
#
# The end product is a macOS app, but on Linux we can still:
#   * `cargo check` / `clippy` / `test` the Rust crate in `src-tauri/`
#     (matches the `rust` job in `.github/workflows/devcontainers-app.yml`),
#   * `deno task check` / `build` the engine bundle (matches the `deno` job),
#   * `yarn --cwd frontend` build the Vite frontend,
#   * `npx tsc -p spec/tsconfig.json --noEmit` for the spec slice.
#
# Rust + Tauri system deps are baked in here. Deno and Node are layered on
# via devcontainer features in `devcontainer.json`.

ARG VARIANT="bookworm"
FROM mcr.microsoft.com/devcontainers/rust:1-${VARIANT}

# Tauri 2 Linux system dependencies (webkit2gtk 4.1).
# Mirrors the install step in `.github/workflows/devcontainers-app.yml`.
RUN apt-get update \
	&& export DEBIAN_FRONTEND=noninteractive \
	&& apt-get install -y --no-install-recommends \
		libwebkit2gtk-4.1-dev \
		libgtk-3-dev \
		libayatana-appindicator3-dev \
		librsvg2-dev \
		libsoup-3.0-dev \
		libjavascriptcoregtk-4.1-dev \
		patchelf \
		file \
		pkg-config \
		build-essential \
		curl \
		wget \
		ca-certificates \
		xdg-utils \
	&& apt-get clean \
	&& rm -rf /var/lib/apt/lists/*

ARG USERNAME=vscode

# Pre-install commonly used Rust components and the Tauri CLI.
USER $USERNAME
RUN rustup component add clippy rustfmt \
	&& cargo install tauri-cli --version "^2.0" --locked

# Save command line history across container rebuilds.
RUN echo "export HISTFILE=/home/$USERNAME/commandhistory/.bash_history" >> "/home/$USERNAME/.bashrc" \
	&& echo "export PROMPT_COMMAND='history -a'" >> "/home/$USERNAME/.bashrc" \
	&& mkdir -p /home/$USERNAME/commandhistory \
	&& touch /home/$USERNAME/commandhistory/.bash_history

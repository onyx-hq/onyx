#!/bin/bash

REPO="onyx-hq/onyx"
INSTALL_DIR="$HOME/.local/bin"

# Ensure the install directory exists
mkdir -p "$INSTALL_DIR"

# Get the version to install from the environment, default to the latest release tag if not provided
VERSION=${ONYX_VERSION:-latest}

# Determine the OS and architecture
OS=$(uname | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

# Ensure the install directory is in the PATH
if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
	echo "The install directory is not in your PATH. Adding it now..."
	SHELL_NAME=$(basename "$SHELL")
	case "$SHELL_NAME" in
	bash)
		echo "export PATH=\$PATH:$INSTALL_DIR" >>"$HOME/.bashrc"
		source "$HOME/.bashrc"
		;;
	zsh)
		echo "export PATH=\$PATH:$INSTALL_DIR" >>"$HOME/.zshrc"
		source "$HOME/.zshrc"
		;;
	*)
		echo "Unsupported shell: $SHELL_NAME. Please add $INSTALL_DIR to your PATH manually before installing this tool"
		;;
	esac
fi

# Map architecture to target
case $ARCH in
x86_64)
	if [ "$OS" == "darwin" ]; then
		TARGET="x86_64-apple-darwin"
	else
		TARGET="x86_64-unknown-linux-gnu"
	fi
	;;
aarch64 | arm64)
	if [ "$OS" == "darwin" ]; then
		TARGET="aarch64-apple-darwin"
	else
		TARGET="aarch64-unknown-linux-gnu"
	fi
	;;
*)
	echo "Unsupported architecture: $ARCH"
	exit 1
	;;
esac

# Download the release binary
if [ "$VERSION" == "latest" ]; then
	BINARY_URL="https://github.com/$REPO/releases/latest/download/onyx-$TARGET"
else
	BINARY_URL="https://github.com/$REPO/releases/download/$VERSION/onyx-$TARGET"
fi

curl -L $BINARY_URL -o onyx-$TARGET

# Make the binary executable
chmod +x onyx-$TARGET

# Move the binary to the install directory
mv onyx-$TARGET $INSTALL_DIR/onyx

echo "Onyx version $VERSION for $TARGET has been installed successfully!"

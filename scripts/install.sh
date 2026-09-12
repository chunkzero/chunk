#!/bin/sh
set -eu

version=${1:?Usage: install.sh VERSION [ARCHIVE]}
case "$version" in
  *[!0-9A-Za-z.-]* | .* | *..* | "") echo "Invalid version" >&2; exit 1 ;;
esac
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then
  echo "The initial Chunk SDK supports Linux x64." >&2
  exit 1
fi

prefix=${CHUNK_INSTALL_DIR:-"$HOME/.local"}
case "$prefix" in
  /*) ;;
  *) prefix="$(pwd)/$prefix" ;;
esac
destination="$prefix/share/chunk/$version"
if [ -e "$destination" ] || [ -L "$destination" ]; then
  echo "Already installed: $destination" >&2
  exit 1
fi
if [ -e "$prefix/bin/chunk" ] && [ ! -L "$prefix/bin/chunk" ]; then
  echo "Refusing to replace $prefix/bin/chunk; choose another CHUNK_INSTALL_DIR." >&2
  exit 1
fi

name="chunk-$version-linux-x64"
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
trap 'exit 1' HUP INT TERM
if [ "$#" -ge 2 ]; then
  cp "$2" "$temporary/$name.tar.gz"
  cp "$2.sha256" "$temporary/$name.tar.gz.sha256"
else
  url="https://github.com/chunkzero/chunk/releases/download/v$version"
  curl --fail --location --proto '=https' --tlsv1.2 "$url/$name.tar.gz" -o "$temporary/$name.tar.gz"
  curl --fail --location --proto '=https' --tlsv1.2 "$url/$name.tar.gz.sha256" -o "$temporary/$name.tar.gz.sha256"
fi

cd "$temporary"
expected=$(cut -d ' ' -f 1 "$name.tar.gz.sha256")
actual=$(sha256sum "$name.tar.gz" | cut -d ' ' -f 1)
if [ "$expected" != "$actual" ]; then
  echo "SDK checksum does not match." >&2
  exit 1
fi
tar -xzf "$name.tar.gz"
if [ "$("$temporary/$name/chunk" --version)" != "chunk $version" ]; then
  echo "SDK version does not match." >&2
  exit 1
fi
mkdir -p "$prefix/share/chunk" "$prefix/bin"
mv "$name" "$destination"
ln -sfn "$destination/chunk" "$prefix/bin/chunk"
echo "Installed Chunk $version. Add $prefix/bin to PATH."

#!/bin/sh
# Regenerates the fake yt-dlp releases used by tests: two versions of a fake
# binary, their SHA2-256SUMS signed with a throwaway key (its private half is
# discarded), and a SUMS file with a wrong hash, signed with the same key.
# Requires gpg. Run from the repository root.
set -eu
out=tests/fixtures/ytdlp/fake
names="yt-dlp_linux yt-dlp_linux_aarch64 yt-dlp_musllinux yt-dlp_musllinux_aarch64 yt-dlp_macos"
export GNUPGHOME="$(mktemp -d)"
trap 'rm -rf "$GNUPGHOME"' EXIT
gpg --batch --quiet --passphrase '' --quick-gen-key "tidepool test signing key" ed25519 sign never
gpg --batch --armor --export > "$out/test-signing-key.asc"
for version in 2026.01.01 2026.02.01; do
  dir="$out/$version"
  mkdir -p "$dir"
  printf '#!/bin/sh\necho %s\n' "$version" > "$dir/yt-dlp"
  sum=$(shasum -a 256 "$dir/yt-dlp" | cut -d' ' -f1)
  : > "$dir/SHA2-256SUMS"
  for name in $names; do printf '%s  %s\n' "$sum" "$name" >> "$dir/SHA2-256SUMS"; done
  gpg --batch --yes --detach-sign -o "$dir/SHA2-256SUMS.sig" "$dir/SHA2-256SUMS"
done
dir="$out/tampered"
mkdir -p "$dir"
cp "$out/2026.02.01/yt-dlp" "$dir/yt-dlp"
: > "$dir/SHA2-256SUMS"
for name in $names; do printf '%064d  %s\n' 0 "$name" >> "$dir/SHA2-256SUMS"; done
gpg --batch --yes --detach-sign -o "$dir/SHA2-256SUMS.sig" "$dir/SHA2-256SUMS"

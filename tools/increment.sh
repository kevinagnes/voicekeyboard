#!/bin/bash
# Incremental release pipeline: bump version -> build -> sign -> notarize -> GitHub release.
#
# Usage:  cargo increment [-v p|m|M] [--dry-run] [--no-push]
#   -v p   patch bump (default)   0.1.0 -> 0.1.1
#   -v m   minor bump             0.1.0 -> 0.2.0
#   -v M   major bump             0.1.0 -> 1.0.0
#   --dry-run   show the new version and exit (no changes)
#   --no-push   build + tag locally but skip push / GitHub release
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Optional local-only overrides (never committed): GH_TOKEN, VK_* vars, etc.
if [[ -f "$ROOT/.local.env" ]]; then
  set -a; source "$ROOT/.local.env"; set +a
fi

BUMP="p"
PUSH=1
shift # drop the "increment" subcommand name passed by cargo
for arg in "$@"; do
  case "$arg" in
    -v) ;;
    p|m|M) BUMP="$arg" ;;
    --dry-run) DRY_RUN=1 ;;
    --no-push) PUSH=0 ;;
    --release) ;; # always a release build
    *) echo "Unknown argument: $arg"; exit 2 ;;
  esac
done

CARGO_MANIFEST="src-tauri/Cargo.toml"
TAURI_CONF="src-tauri/tauri.conf.json"

CUR="$(sed -n 's/^version = "\(.*\)"/\1/p' "$CARGO_MANIFEST")"
IFS=. read -r MAJ MIN PAT <<< "$CUR"
case "$BUMP" in
  p) PAT=$((PAT + 1)) ;;
  m) MIN=$((MIN + 1)); PAT=0 ;;
  M) MAJ=$((MAJ + 1)); MIN=0; PAT=0 ;;
esac
NEW="$MAJ.$MIN.$PAT"

echo "==> Current version: $CUR  ->  $NEW (bump: $BUMP)"
if [[ "${DRY_RUN:-0}" == "1" ]]; then
  echo "==> Dry run, nothing changed."
  exit 0
fi

if [[ -n "$(git status --porcelain)" ]]; then
  echo "!! Working tree is dirty. Commit or stash changes first:" >&2
  git status --porcelain >&2
  exit 1
fi

echo "==> Bumping version in $CARGO_MANIFEST and $TAURI_CONF"
sed -i '' "s/^version = \"$CUR\"/version = \"$NEW\"/" "$CARGO_MANIFEST"
sed -i '' "s/\"version\": \"$CUR\"/\"version\": \"$NEW\"/" "$TAURI_CONF"

echo "==> Building, signing, notarizing (tools/package.sh)…"
"$ROOT/tools/package.sh"
DMG="$ROOT/dist/VoiceKeyboard.dmg"
if [[ ! -f "$DMG" ]]; then
  echo "!! Build did not produce $DMG" >&2
  exit 1
fi

git add "$CARGO_MANIFEST" "$TAURI_CONF"
git commit -m "release: v$NEW"
git tag "v$NEW"

if [[ "$PUSH" == "1" ]]; then
  echo "==> Pushing main + tag v$NEW"
  git push origin main
  git push origin "v$NEW"

  echo "==> Creating GitHub release"
  gh release create "v$NEW" "$DMG" \
    --title "VoiceKeyboard v$NEW" \
    --notes "Automated release built via \`cargo increment -v $BUMP\`." \
    --generate-notes
  echo "==> Done: https://github.com/kevinagnes/voicekeyboard/releases/tag/v$NEW"
else
  echo "==> Local only: tag v$NEW created, nothing pushed."
fi

#!/usr/bin/env bash
# Copies the Homebrew libraries an app executable links to into its bundle.
#
# Bash rather than POSIX sh: the recursive helper relies on `local`.
set -euo pipefail

homebrew_path=""
exec_path="target/debug/bundle/osx/Syntra.app/Contents/MacOS/syntra"

usage() {
    cat <<EOF
$0: Copy all Homebrew libraries into the macOS app bundle.
USAGE: $0 [-h] [-b homebrew_path] [exec_path]

OPTIONS:
  -h, --help    Show this help message and exit
  -b, --homebrew PATH
                Path to the Homebrew installation
                (default: the output of \`brew --prefix\`)
  exec_path     Path to the main executable in the app bundle
                (default: $exec_path)

When macOS apps are linked to dynamic libraries (.dylib files),
the fully qualified path to the library is embedded in the binary.
If the libraries come from Homebrew, that means that Homebrew must be present
and the libraries must be installed in the same location on the user's machine.

This script copies all of the Homebrew libraries that an executable links to into the app bundle
and tells all the binaries in the bundle to look for them there.

Nothing is created or changed unless the executable exists and the required
macOS tools (otool, install_name_tool, codesign) are available.
EOF
}

die() {
    echo "$0: $*" >&2
    exit 1
}

# Gather command-line arguments.
while [ $# -gt 0 ]; do
    case "$1" in
        -h | --help) usage; exit 0 ;;
        -b | --homebrew)
            [ $# -ge 2 ] || die "$1 needs a path"
            homebrew_path="$2"
            shift 2
            ;;
        -*) die "unknown option: $1 (see --help)" ;;
        *) exec_path="$1"; shift ;;
    esac
done

# Validate everything before the first change to disk: a wrong path must leave
# the bundle exactly as it was.
[ -f "$exec_path" ] || die "missing bundled executable: $exec_path"
[ -x "$exec_path" ] || die "bundled executable is not executable: $exec_path"
for tool in otool install_name_tool codesign; do
    command -v "$tool" >/dev/null 2>&1 || die "required tool not found: $tool (this script runs on macOS)"
done
if [ -z "$homebrew_path" ]; then
    command -v brew >/dev/null 2>&1 || die "Homebrew not found; pass -b PATH"
    homebrew_path="$(brew --prefix)"
fi
[ -d "$homebrew_path" ] || die "Homebrew path is not a directory: $homebrew_path"

# Path to the .app bundle
bundle_path=$(dirname "$(dirname "$(dirname "$exec_path")")")
# Path to the Frameworks directory
fwks_path="$bundle_path/Contents/Frameworks"
# Path to bundled resources
resources_path="$bundle_path/Contents/Resources"

mkdir -p "$fwks_path"

# Copy and fix references for a binary (executable or dylib)
#
# This function will:
# - Copy any referenced dylibs from the Homebrew prefix to the Frameworks directory
# - Update the binary to reference the local copy instead
# - Recursively process the copied dylibs
fix_references() {
    local bin="$1"
    local libs old_path base_name dest

    # Get all Homebrew libraries referenced by the binary. The prefix is
    # matched literally at the start of the path, not as a pattern.
    libs=$(otool -L "$bin" | awk -v homebrew="$homebrew_path" 'index($1, homebrew) == 1 {print $1}')

    while IFS= read -r old_path; do
        [ -n "$old_path" ] || continue

        base_name="$(basename "$old_path")"
        dest="$fwks_path/$base_name"

        if [ ! -e "$dest" ]; then
            echo "Copying $old_path -> $dest"
            cp -f "$old_path" "$dest"
            # Ensure the copied dylib is writable so that `xattr -rd` on the
            # app bundle works.
            chmod 644 "$dest"

            echo "Updating $dest to have install_name of @rpath/$base_name..."
            install_name_tool -id "@rpath/$base_name" "$dest"

            # Recursively process this dylib
            fix_references "$dest"
        fi

        echo "Updating $bin to reference @rpath/$base_name..."
        install_name_tool -change "$old_path" "@rpath/$base_name" "$bin"
    done <<<"$libs"
}

fix_references "$exec_path"

# cargo-bundle preserves the source path under Contents/Resources (so
# `target/menubar-template.png` lands at `Resources/target/...`). Flatten it
# so NSBundle pathForResource: finds the file at the Resources root.
if [ -f "$resources_path/target/menubar-template.png" ]; then
    mv "$resources_path/target/menubar-template.png" "$resources_path/menubar-template.png"
    rmdir "$resources_path/target" 2>/dev/null || true
fi

# Ensure the main executable has our Frameworks path in its RPATH
if ! otool -l "$exec_path" | grep -q "@executable_path/../Frameworks"; then
    echo "Adding RPATH to $exec_path"
    install_name_tool -add_rpath "@executable_path/../Frameworks" "$exec_path"
fi

# Ad-hoc sign the .app
codesign --force --deep --sign - "$bundle_path"

echo "Done!"

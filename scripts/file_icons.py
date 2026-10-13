#!/usr/bin/env python3
"""Pick the SFTP list's color icons out of Material Icon Theme (MIT).

ShellRS ships a part of the theme: the icons in ICONS below, each with every
file name, extension and folder name the theme gives it. This writes them to
assets/icons/files/ (with the theme's license) and the lookup tables to
src/explorer/file_icons_table.rs, then formats the tables with rustfmt.

    npm pack material-icon-theme@<version>
    mkdir material && tar xzf material-icon-theme-<version>.tgz -C material
    python3 -I scripts/file_icons.py material/package

Run it again with a newer package to take the theme's new associations.
"""

import json
import re
import subprocess
import sys
from pathlib import Path

# The icons ShellRS ships: what is common on servers, in projects deployed to
# them and in home folders. A name or extension the theme gives an icon not
# listed here gets the plain file or folder.
ICONS = [
    # Files.
    "file", "document", "log", "console", "settings", "tune", "json", "yaml",
    "xml", "toml", "markdown", "readme", "license", "changelog", "authors",
    "contributing", "table", "pdf", "word", "powerpoint", "epub", "email",
    "zip", "jar", "javaclass", "image", "svg", "audio", "video", "subtitles",
    "font", "key", "certificate", "lock", "database", "exe", "dll", "lib",
    "hex", "disc", "docker", "git", "diff", "nginx", "hosts", "systemd", "vim",
    "python", "python-misc", "jupyter", "javascript", "typescript", "react",
    "react_ts", "nodejs", "npm", "yarn", "vue", "html", "css", "sass", "less",
    "java", "kotlin", "scala", "gradle", "maven", "go", "go-mod", "rust", "c",
    "cpp", "h", "hpp", "csharp", "php", "ruby", "gemfile", "lua", "perl", "r",
    "swift", "dart", "powershell", "makefile", "cmake", "terraform", "hcl",
    "nix", "proto", "graphql", "android", "jinja", "editorconfig", "vagrant",
    "jenkins", "robots", "favicon",
    # Folders.
    "folder", "folder-config", "folder-scripts", "folder-secure", "folder-keys",
    "folder-private", "folder-git", "folder-github", "folder-home",
    "folder-temp", "folder-log", "folder-src", "folder-lib", "folder-dist",
    "folder-target", "folder-test", "folder-docs", "folder-images",
    "folder-audio", "folder-video", "folder-download", "folder-upload",
    "folder-desktop", "folder-public", "folder-database", "folder-backup",
    "folder-archive", "folder-trash", "folder-node", "folder-python",
    "folder-environment", "folder-docker", "folder-kubernetes", "folder-helm",
    "folder-ansible", "folder-terraform", "folder-nginx", "folder-redis",
    "folder-server", "folder-client", "folder-api", "folder-app",
    "folder-packages", "folder-plugin", "folder-include", "folder-resource",
    "folder-views", "folder-template", "folder-font", "folder-mail",
    "folder-tools", "folder-utils", "folder-vscode", "folder-intellij",
    "folder-go", "folder-rust", "folder-gradle", "folder-project",
    "folder-link", "folder-other",
]

# What VS Code finds by a file's language rather than the theme's tables, for
# the files a server has: systemd units (the systemd extension's language)
# and the SSH and vim files. The theme's own entries win over these.
EXTRA_FILE_EXTENSIONS = {
    **{ext: "systemd" for ext in [
        "service", "socket", "timer", "mount", "automount", "slice", "target",
    ]},
}
EXTRA_FILE_NAMES = {
    "ssh_config": "lock",
    "sshd_config": "lock",
    **{name: "key" for name in [
        "authorized_keys", "known_hosts", "id_rsa", "id_dsa", "id_ecdsa",
        "id_ed25519",
    ]},
    **{name: "vim" for name in [".vimrc", "_vimrc", ".gvimrc", ".exrc", ".viminfo"]},
}
EXTRA_FOLDER_NAMES = {
    ".ssh": "folder-secure",
    ".gnupg": "folder-secure",
}

# The plain folder's color: yellow, as in Windows, WinSCP and Xftp, rather
# than the theme's blue-grey, which reads as one more grey icon in a list that
# is mostly plain folders. The theme offers the same choice in VS Code
# (material-icon-theme.folders.color); folders with a motif keep theirs.
FOLDER_COLOR = "#ffca28"
THEME_FOLDER_COLOR = "#90a4ae"

# Folder names whose meaning in a project misleads on a server: /dev holds
# devices, not development, and /media mount points, not videos.
EXCLUDED_FOLDER_NAMES = {"dev", "media"}

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets" / "icons" / "files"
TABLE = ROOT / "src" / "explorer" / "file_icons_table.rs"
ASSET_PREFIX = "icons/files/"


def main(package: Path) -> None:
    manifest = json.loads((package / "dist" / "material-icons.json").read_text())
    version = json.loads((package / "package.json").read_text())["version"]
    definitions = manifest["iconDefinitions"]
    missing = [icon for icon in ICONS if icon not in definitions]
    if missing:
        sys.exit(f"not in the theme: {', '.join(missing)}")
    shipped = set(ICONS)

    def base(icon: str) -> str:
        return icon.removesuffix("_light")

    def associations(table: dict, extra: dict, excluded=frozenset()) -> dict:
        found = {
            key.lower(): icon
            for key, icon in table.items()
            if base(icon) in shipped and "/" not in key and key.lower() not in excluded
        }
        for key, icon in extra.items():
            found.setdefault(key, icon)
        return dict(sorted(found.items()))

    light = manifest["light"]
    tables = {
        "FOLDER_NAMES": associations(
            manifest["folderNames"], EXTRA_FOLDER_NAMES, EXCLUDED_FOLDER_NAMES
        ),
        "FOLDER_NAMES_LIGHT": associations(
            light.get("folderNames", {}), {}, EXCLUDED_FOLDER_NAMES
        ),
        "FILE_NAMES": associations(manifest["fileNames"], EXTRA_FILE_NAMES),
        "FILE_NAMES_LIGHT": associations(light.get("fileNames", {}), {}),
        "FILE_EXTENSIONS": associations(
            manifest["fileExtensions"], EXTRA_FILE_EXTENSIONS
        ),
        "FILE_EXTENSIONS_LIGHT": associations(light.get("fileExtensions", {}), {}),
    }
    used = sorted(
        {manifest["file"], manifest["folder"]}
        | {icon for table in tables.values() for icon in table.values()}
    )

    ASSETS.mkdir(parents=True, exist_ok=True)
    for old in ASSETS.glob("*.svg"):
        old.unlink()
    for icon in used:
        source = (package / "dist" / definitions[icon]["iconPath"]).resolve()
        svg = sized(source.read_text())
        if icon == manifest["folder"]:
            if THEME_FOLDER_COLOR not in svg:
                sys.exit("the plain folder's color has changed")
            svg = svg.replace(THEME_FOLDER_COLOR, FOLDER_COLOR)
        (ASSETS / f"{icon}.svg").write_text(svg)
    (ASSETS / "LICENSE").write_text((package / "LICENSE").read_text())

    TABLE.write_text(rust_tables(version, manifest, tables, used))
    subprocess.run(["rustfmt", "--edition", "2024", str(TABLE)], check=True)
    print(f"{len(used)} icons from Material Icon Theme {version}")


def sized(svg: str) -> str:
    """The icon drawn at 16 px. GPUI renders an SVG image at twice its own
    size, so a 16-px icon is sharp on a 2× screen and halves cleanly on a 1×
    one, whatever its view box."""
    svg = svg.strip()
    match = re.match(r"<svg\b([^>]*)>", svg)
    if not match:
        sys.exit("not an SVG")
    attributes = re.sub(r'\s(width|height)="[^"]*"', "", match.group(1))
    return f'<svg width="16" height="16"{attributes}>{svg[match.end():]}\n'


def rust_tables(version: str, manifest: dict, tables: dict, used: list) -> str:
    def path(icon: str) -> str:
        return f'"{ASSET_PREFIX}{icon}.svg"'

    lines = [
        f"//! Generated by `scripts/file_icons.py` from Material Icon Theme {version}",
        "//! (MIT, assets/icons/files/LICENSE). Do not edit by hand.",
        "//!",
        "//! Keys are lowercase, sorted for a binary search. A `_LIGHT` table",
        "//! holds the icons that change on a light background.",
        "",
        f"pub(super) const DEFAULT_FILE: &str = {path(manifest['file'])};",
        f"pub(super) const DEFAULT_FOLDER: &str = {path(manifest['folder'])};",
        "",
    ]
    for name, table in tables.items():
        lines.append(f"pub(super) const {name}: &[(&str, &str)] = &[")
        lines += [f"    ({json.dumps(key, ensure_ascii=False)}, {path(icon)})," for key, icon in table.items()]
        lines += ["];", ""]
    lines.append("/// The icons, for the application's asset source.")
    lines.append("pub const FILE_ICON_ASSETS: &[(&str, &[u8])] = &[")
    for icon in used:
        lines.append(
            f'    ({path(icon)}, include_bytes!("../../assets/icons/files/{icon}.svg")),'
        )
    lines += ["];", ""]
    return "\n".join(lines)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(Path(sys.argv[1]))

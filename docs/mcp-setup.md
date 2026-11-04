# Using cleanmcp with AI agents (MCP)

`cleanmcp mcp` starts a [Model Context Protocol](https://modelcontextprotocol.io) server. By default it
talks over stdio and every tool is **read-only**.

## Options

| Flag | Effect |
|---|---|
| `--root DIR` | Only allow paths inside `DIR` (repeatable). The root folder itself cannot be trashed. Recommended when you enable deletion. `list_drives` still reports every volume. |
| `--allow-delete` | Adds the `move_to_trash` tool. Items go to the Recycle Bin / Trash (recoverable). It only accepts paths an earlier tool returned in the same session, and always refuses drive roots, your home folder (or any folder containing it), system folders, other users' profiles and the `--root` folders themselves. Attempts that reach the trash step are logged; see below. |
| `--http ADDR` | Serve Streamable HTTP at `http://ADDR/mcp` instead of stdio, e.g. `--http 127.0.0.1:8765`. There is no authentication: keep it on localhost. `--allow-delete` is refused unless `ADDR` is a loopback address. |
| `--config FILE` | Use a specific config file (custom cleanup rules, excludes). A missing file is an error. |

Deletion attempts that reach the trash step are appended as JSON lines to
`%LOCALAPPDATA%\cleanmcp\deletions.log` on Windows or
`~/.local/share/cleanmcp/deletions.log` on Linux (`$CLEANMCP_DATA_DIR` overrides the folder). The
log includes the optional `reason`, the cached size and failed attempts. Requests refused before
the trash step (outside `--root`, not returned by an earlier tool, or not a deletable path) are not
logged.

Scan and query tools resolve their path first (symlinks, `..`, letter case). A symlink passed as a
scan root is followed once; symlinks and junctions found *inside* a scan are recorded as links and
never followed. `move_to_trash` resolves only the parent folder, so it trashes a link itself, never
its target.

Use the full path to the binary if `cleanmcp` is not on your `PATH`
(e.g. `C:\\Tools\\cleanmcp.exe` in JSON, with doubled backslashes).

## Claude Code

```sh
claude mcp add --scope user cleanmcp -- cleanmcp mcp
# with deletion, limited to your home folder ($HOME expands in POSIX shells;
# on Windows use the full path, e.g. C:\Users\me):
claude mcp add --scope user cleanmcp -- cleanmcp mcp --allow-delete --root "$HOME"
```

## Claude Desktop

Edit `%APPDATA%\Claude\claude_desktop_config.json` (Windows) or
`~/.config/Claude/claude_desktop_config.json` (Linux) and restart Claude Desktop:

```json
{
  "mcpServers": {
    "cleanmcp": { "command": "C:\\Tools\\cleanmcp.exe", "args": ["mcp"] }
  }
}
```

## VS Code (GitHub Copilot agent mode)

`.vscode/mcp.json` in a workspace, or the user-level MCP configuration:

```json
{
  "servers": {
    "cleanmcp": { "type": "stdio", "command": "cleanmcp", "args": ["mcp"] }
  }
}
```

## Cursor

`~/.cursor/mcp.json` (all projects) or `.cursor/mcp.json` (one project):

```json
{
  "mcpServers": {
    "cleanmcp": { "command": "cleanmcp", "args": ["mcp"] }
  }
}
```

## Over HTTP

```sh
cleanmcp mcp --http 127.0.0.1:8765
```

Point any client that supports Streamable HTTP at `http://127.0.0.1:8765/mcp`.

## Tools

| Tool | What it does |
|---|---|
| `list_drives` | Volumes with total / used / free space (not limited by `--root`); `include_virtual` also lists pseudo filesystems |
| `scan_directory` | Scan a folder or drive (cached), returning totals plus the largest 15 direct children and counts for the rest; `refresh`, `one_filesystem`, `apparent`; reports progress when the client supplies a progress token, and can be cancelled |
| `list_children` | Drill into a folder, largest first; `depth` 1-4, `top` (default 20, max 200; lowered when `depth > 1`), `offset` for the first level, `apparent`; `other_count` / `other_disk_size` report what is left |
| `largest_files` | Biggest files with `min_size_mib`, `older_than_days`, `newer_than_days`, `extensions`, `apparent`; `top` (default 25, max 500); `next_offset` for pagination |
| `extension_breakdown` | Usage per file type; `top` (default 25, max 500), `apparent` |
| `age_breakdown` | Usage by last-modified age, including an `unknown` bucket |
| `find_cleanup_candidates` | Folders that are usually safe to remove, with safety level, reason and cleanup command; `min_size_mib` (default 10), `top` (default 30, max 500), `apparent`; `next_offset` |
| `find_duplicates` | Identical files, most wasted space first; `min_size_mib` (default 1), `top` (default 20, max 200), `offset`, `apparent`; `next_offset`; each page re-hashes the candidates |
| `save_snapshot` | Scan a folder and write a snapshot file (`.gz` = compressed) inside the `--root` folders, for later comparison |
| `diff_snapshots` | Compare a snapshot with a newer snapshot, a folder scanned now, or the snapshot's own root rescanned; `depth` (default 3, max 6), `top`, `apparent`; largest absolute change first |
| `move_to_trash` | Only with `--allow-delete`; `paths` and an optional `reason`; see above |

Resource: `cleanmcp://scans` lists the scans cached by the server process (up to four; over HTTP the
cache is shared by all sessions, while the "paths seen" list that `move_to_trash` checks is per
session).

## Example prompt

> My C: drive is almost full. Use cleanmcp to find what is taking space and what I can safely
> clean up. Show me a list with sizes before deleting anything.

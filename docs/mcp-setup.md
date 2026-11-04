# Using cleanmcp with AI agents (MCP)

`cleanmcp mcp` starts a [Model Context Protocol](https://modelcontextprotocol.io) server. By default it
talks over stdio and every tool is **read-only**.

## Options

| Flag | Effect |
|---|---|
| `--root DIR` | Only allow paths inside `DIR` (repeatable). Recommended when you enable deletion. |
| `--allow-delete` | Adds the `move_to_trash` tool. Items go to the Recycle Bin / Trash (recoverable). It only accepts paths an earlier tool returned in the same session, and always refuses drive roots, your home folder, system folders and other users' profiles. Every attempt is logged to `%LOCALAPPDATA%\cleanmcp\deletions.log` / `~/.local/share/cleanmcp/deletions.log`. |
| `--http ADDR` | Serve Streamable HTTP at `http://ADDR/mcp` instead of stdio, e.g. `--http 127.0.0.1:8765`. There is no authentication: keep it on localhost. |
| `--config FILE` | Use a specific config file (custom cleanup rules, excludes). |

Use the full path to the binary if `cleanmcp` is not on your `PATH`
(e.g. `C:\\Tools\\cleanmcp.exe` in JSON, with doubled backslashes).

## Claude Code

```sh
claude mcp add --scope user cleanmcp -- cleanmcp mcp
# with deletion, limited to your home folder:
claude mcp add --scope user cleanmcp -- cleanmcp mcp --allow-delete --root ~
```

## Claude Desktop

Edit `%APPDATA%\Claude\claude_desktop_config.json` (Windows) and restart Claude Desktop:

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
| `list_drives` | Volumes with total / used / free space |
| `scan_directory` | Scan a folder or drive (cached); sends progress notifications and can be cancelled |
| `list_children` | Drill into a folder, largest first; `depth`, `top`, `offset` |
| `largest_files` | Biggest files with `min_size_mib`, `older_than_days`, `newer_than_days`, `extensions`; paginated |
| `extension_breakdown` | Usage per file type |
| `age_breakdown` | Usage by last-modified age |
| `find_cleanup_candidates` | Folders that are usually safe to remove, with safety level, reason and cleanup command; paginated |
| `find_duplicates` | Identical files, most wasted space first |
| `move_to_trash` | Only with `--allow-delete`; see above |

Resource: `cleanmcp://scans` lists the scans cached in the current session.

## Example prompt

> My C: drive is almost full. Use cleanmcp to find what is taking space and what I can safely
> clean up. Show me a list with sizes before deleting anything.

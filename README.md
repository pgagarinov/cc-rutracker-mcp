# RuTracker MCP Server

An [MCP](https://modelcontextprotocol.io/) server that lets Claude search and browse [rutracker.org](https://rutracker.org) torrents. Authentication is handled automatically via cookies from the Brave browser.

## Tools

| Tool | Description |
|---|---|
| `search` | Search for torrents by query, with optional category, sort, and order |
| `get_topic` | Get detailed info about a torrent: title, size, seeds, magnet link, description, file list |

## Prerequisites

- **macOS** (osx-arm64)
- **[Pixi](https://pixi.sh/)** package manager
- **Brave browser** with an active rutracker.org session in a profile named "Peter"

## Setup

```bash
# Install dependencies
pixi install

# Run the server (stdio transport)
pixi run serve
```

## Usage with Claude Code

The included `.mcp.json` configures the server automatically. Just open Claude Code in this directory and the `search` / `get_topic` tools will be available.

## Usage with Claude Desktop

Add to `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "rutracker": {
      "command": "pixi",
      "args": ["run", "--manifest-path", "/path/to/this/repo/pyproject.toml", "serve"]
    }
  }
}
```

## How it works

1. On startup, cookies are loaded from Brave's cookie store (cached locally in `.cookies.json` to avoid repeated Keychain prompts)
2. Search queries go to `tracker.php`, topic lookups go to `viewtopic.php`
3. HTML responses (cp1251 encoded) are parsed with BeautifulSoup
4. If a login redirect is detected, cookies are auto-refreshed from Brave

# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

MCP (Model Context Protocol) server for RuTracker integration. Lets Claude search and browse rutracker.org using auth cookies from the Brave browser. Python 3.11+.

## Development Environment

Uses **Pixi** (conda-based) for environment management and **Hatchling** as the build backend.

```bash
# Set up environment (installs dependencies + package in editable mode)
pixi install

# Run the MCP server
pixi run serve
```

## Architecture

```
src/rutracker_mcp/
    __init__.py      # re-exports main()
    __main__.py      # python -m entry point
    server.py        # FastMCP server, tool definitions, lifespan
    cookies.py       # Brave profile resolution + pycookiecheat extraction
    client.py        # Async httpx client for rutracker.org
    parser.py        # BeautifulSoup HTML parsing (search results, topic pages)
```

- `server.py` — FastMCP server with lifespan pattern; defines `search` and `get_topic` tools
- `cookies.py` — reads cookies from Brave profile "Peter" via pycookiecheat; caches to `.cookies.json`
- `client.py` — async httpx wrapper handling cp1251 encoding and login detection
- `parser.py` — BeautifulSoup parsers for tracker.php and viewtopic.php pages

## Key Dependencies

`mcp`, `httpx`, `beautifulsoup4`, `lxml`, `pycookiecheat`

## Configuration

- Platform target: osx-arm64, conda-forge channel
- Brave profile: "Peter" (Profile 2) — must have an active rutracker.org session
- RuTracker pages use cp1251 encoding
- Cookies are cached in `.cookies.json` (gitignored) to avoid repeated Keychain prompts
- On login redirect, cookies are auto-refreshed from Brave (one-time Keychain prompt)

## MCP Setup

The `.mcp.json` in the project root configures Claude Code to use this server automatically when working in this directory.

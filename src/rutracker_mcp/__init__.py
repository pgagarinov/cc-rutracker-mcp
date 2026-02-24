"""RuTracker MCP server — search and browse rutracker.org via MCP."""

from .server import mcp


def main() -> None:
    """Run the MCP server with stdio transport."""
    mcp.run(transport="stdio")

"""FastMCP server for RuTracker integration."""

from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from dataclasses import dataclass

import httpx
from mcp.server.fastmcp import Context, FastMCP

from .client import RuTrackerClient


@dataclass
class AppContext:
    client: RuTrackerClient


@asynccontextmanager
async def app_lifespan(server: FastMCP) -> AsyncIterator[AppContext]:
    """Manage RuTracker client lifecycle."""
    async with httpx.AsyncClient(
        follow_redirects=True,
        timeout=30.0,
        headers={"User-Agent": "Mozilla/5.0"},
    ) as http_client:
        client = RuTrackerClient(http_client)
        client.load_cookies()
        yield AppContext(client=client)


mcp = FastMCP("RuTracker MCP", lifespan=app_lifespan)


@mcp.tool()
async def search(
    query: str,
    ctx: Context,
    category: str | None = None,
    sort_by: str = "seeders",
    order: str = "desc",
) -> str:
    """Search RuTracker for torrents.

    Args:
        query: Search query string (Russian or English).
        category: Optional forum/category ID to filter results.
        sort_by: Sort field — "seeders", "size", "downloads", or "registered".
        order: Sort order — "desc" or "asc".
    """
    client = ctx.request_context.lifespan_context.client
    results = await client.search(query, category=category, sort_by=sort_by, order=order)

    if not results:
        return "No results found."

    lines = []
    for r in results:
        parts = [f"[{r.topic_id}] {r.title}"]
        if r.size:
            parts.append(f"Size: {r.size}")
        parts.append(f"Seeds: {r.seeds} | Leeches: {r.leeches}")
        if r.category:
            parts.append(f"Category: {r.category}")
        if r.author:
            parts.append(f"Author: {r.author}")
        lines.append(" | ".join(parts))

    return f"Found {len(results)} results:\n\n" + "\n\n".join(lines)


@mcp.tool()
async def get_topic(topic_id: int, ctx: Context) -> str:
    """Get detailed information about a RuTracker topic/torrent.

    Args:
        topic_id: The numeric topic ID from search results.
    """
    client = ctx.request_context.lifespan_context.client
    details = await client.get_topic(topic_id)

    lines = [f"Title: {details.title}"]

    if details.size:
        lines.append(f"Size: {details.size}")
    lines.append(f"Seeds: {details.seeds} | Leeches: {details.leeches}")

    if details.magnet_link:
        lines.append(f"Magnet: {details.magnet_link}")

    if details.description:
        lines.append(f"\nDescription:\n{details.description}")

    if details.file_list:
        lines.append(f"\nFiles ({len(details.file_list)}):")
        for f in details.file_list[:50]:  # limit to 50 files
            lines.append(f"  - {f}")
        if len(details.file_list) > 50:
            lines.append(f"  ... and {len(details.file_list) - 50} more files")

    return "\n".join(lines)

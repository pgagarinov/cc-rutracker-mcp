"""Async HTTP client for rutracker.org."""

import logging

import httpx

from .cookies import get_rutracker_cookies, refresh_cookies
from .parser import SearchResult, TopicDetails, parse_search_results, parse_topic_page

BASE_URL = "https://rutracker.org/forum"
logger = logging.getLogger(__name__)


class RuTrackerClient:
    """Async client for searching and browsing rutracker.org."""

    def __init__(self, http_client: httpx.AsyncClient) -> None:
        self._http = http_client
        self._cookies_loaded = False

    def load_cookies(self, profile_name: str = "Peter") -> None:
        """Load rutracker cookies from a Brave browser profile."""
        cookies = get_rutracker_cookies(profile_name)
        self._http.cookies.update(cookies)
        self._cookies_loaded = True
        logger.info("Loaded %d cookies from Brave profile %r", len(cookies), profile_name)

    async def _get(self, url: str, params: dict | None = None) -> str:
        """Perform a GET request and decode cp1251 response."""
        resp = await self._http.get(url, params=params)
        resp.raise_for_status()
        html = resp.content.decode("cp1251")

        # Detect login redirect — RuTracker redirects to login.php
        if "login.php" in str(resp.url) or 'id="login-form"' in html[:2000]:
            if self._cookies_loaded:
                # Try refreshing cookies from Brave (bypasses cache)
                logger.warning("Detected login redirect, refreshing cookies from Brave...")
                fresh = refresh_cookies()
                self._http.cookies.update(fresh)
                resp = await self._http.get(url, params=params)
                resp.raise_for_status()
                html = resp.content.decode("cp1251")
                if "login.php" in str(resp.url) or 'id="login-form"' in html[:2000]:
                    raise RuntimeError(
                        "RuTracker login required. Ensure the Brave profile "
                        "has an active rutracker.org session."
                    )
            else:
                raise RuntimeError("Cookies not loaded. Call load_cookies() first.")

        return html

    async def search(
        self,
        query: str,
        category: str | None = None,
        sort_by: str = "seeders",
        order: str = "desc",
    ) -> list[SearchResult]:
        """Search RuTracker for torrents.

        Args:
            query: Search query string.
            category: Optional forum/category ID to filter results.
            sort_by: Sort field — "seeders", "size", "downloads", "registered".
            order: Sort order — "desc" or "asc".
        """
        sort_map = {
            "seeders": "10",
            "size": "7",
            "downloads": "4",
            "registered": "1",
        }
        params: dict[str, str] = {
            "nm": query,
            "o": sort_map.get(sort_by, "10"),
            "s": "1" if order == "asc" else "2",
        }
        if category:
            params["f"] = category

        html = await self._get(f"{BASE_URL}/tracker.php", params=params)
        return parse_search_results(html)

    async def get_topic(self, topic_id: int) -> TopicDetails:
        """Get detailed information about a specific topic/torrent."""
        html = await self._get(
            f"{BASE_URL}/viewtopic.php", params={"t": str(topic_id)}
        )
        details = parse_topic_page(html)
        if not details.topic_id:
            details.topic_id = topic_id
        return details

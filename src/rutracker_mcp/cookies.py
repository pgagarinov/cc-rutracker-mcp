"""Brave browser cookie extraction for rutracker.org."""

import json
import logging
from pathlib import Path

from pycookiecheat import BrowserType, get_cookies

BRAVE_APP_SUPPORT = Path.home() / "Library/Application Support/BraveSoftware/Brave-Browser"
RUTRACKER_URL = "https://rutracker.org"
COOKIE_CACHE = Path(__file__).resolve().parents[2] / ".cookies.json"

logger = logging.getLogger(__name__)


def resolve_brave_profile_dir(profile_name: str) -> Path:
    """Resolve a Brave profile directory from its display name."""
    local_state_path = BRAVE_APP_SUPPORT / "Local State"
    local_state = json.loads(local_state_path.read_text())
    profiles = local_state["profile"]["info_cache"]
    for dir_name, info in profiles.items():
        if info.get("name") == profile_name:
            return BRAVE_APP_SUPPORT / dir_name
    available = [info.get("name") for info in profiles.values()]
    raise ValueError(
        f"Brave profile {profile_name!r} not found. Available: {available}"
    )


def save_cookies(cookies: dict[str, str]) -> None:
    """Save cookies to the cache file."""
    COOKIE_CACHE.write_text(json.dumps(cookies, indent=2))
    logger.info("Saved %d cookies to %s", len(cookies), COOKIE_CACHE)


def load_cached_cookies() -> dict[str, str] | None:
    """Load cookies from cache file, or return None if not cached."""
    if COOKIE_CACHE.exists():
        cookies = json.loads(COOKIE_CACHE.read_text())
        logger.info("Loaded %d cookies from cache", len(cookies))
        return cookies
    return None


def refresh_cookies(profile_name: str = "Peter") -> dict[str, str]:
    """Extract fresh cookies from Brave and update the cache."""
    profile_dir = resolve_brave_profile_dir(profile_name)
    cookie_file = profile_dir / "Cookies"
    cookies = get_cookies(
        RUTRACKER_URL,
        browser=BrowserType.BRAVE,
        cookie_file=cookie_file,
    )
    save_cookies(cookies)
    return cookies


def get_rutracker_cookies(profile_name: str = "Peter") -> dict[str, str]:
    """Get rutracker.org cookies, using cache if available."""
    cached = load_cached_cookies()
    if cached:
        return cached
    return refresh_cookies(profile_name)

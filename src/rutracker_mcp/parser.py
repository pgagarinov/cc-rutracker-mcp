"""HTML parsing for rutracker.org search results and topic pages."""

import re
from dataclasses import dataclass, field

from bs4 import BeautifulSoup, Tag


@dataclass
class SearchResult:
    topic_id: int
    title: str
    size: str = ""
    seeds: int = 0
    leeches: int = 0
    author: str = ""
    category: str = ""


@dataclass
class TopicDetails:
    topic_id: int
    title: str
    magnet_link: str = ""
    size: str = ""
    seeds: int = 0
    leeches: int = 0
    description: str = ""
    file_list: list[str] = field(default_factory=list)


def _int(text: str) -> int:
    """Parse an integer from text, returning 0 on failure."""
    try:
        return int(text.strip().replace(",", "").replace("\xa0", ""))
    except (ValueError, AttributeError):
        return 0


def _text(tag: Tag | None) -> str:
    """Extract stripped text from a tag, returning empty string if None."""
    return tag.get_text(strip=True) if tag else ""


def parse_search_results(html: str) -> list[SearchResult]:
    """Parse search results from tracker.php HTML."""
    soup = BeautifulSoup(html, "lxml")
    results = []

    table = soup.find("table", id="tor-tbl")
    if not table:
        return results

    rows = table.find("tbody").find_all("tr") if table.find("tbody") else []
    for row in rows:
        # Topic link
        link = row.find("a", class_="tLink")
        if not link:
            continue

        href = link.get("href", "")
        topic_id_match = re.search(r"t=(\d+)", str(href))
        if not topic_id_match:
            continue

        topic_id = int(topic_id_match.group(1))
        title = link.get_text(strip=True)

        # Size — look for <a> with class containing "dl-stub" or <td> with class "tor-size"
        size_tag = row.find("td", class_="tor-size")
        size = _text(size_tag.find("u")) if size_tag else ""
        if not size:
            size = _text(size_tag) if size_tag else ""

        # Seeds and leeches
        seeds = _int(_text(row.find("td", class_="seed")))
        if not seeds:
            seeds_b = row.find("b", class_="seedmed")
            seeds = _int(_text(seeds_b))

        leeches = _int(_text(row.find("td", class_="leech")))
        if not leeches:
            leeches_b = row.find("b", class_="leechmed")
            leeches = _int(_text(leeches_b))

        # Author
        author_tag = row.find("a", class_="u-link")
        author = _text(author_tag)

        # Category
        cat_tag = row.find("a", class_="gen")
        category = _text(cat_tag)

        results.append(SearchResult(
            topic_id=topic_id,
            title=title,
            size=size,
            seeds=seeds,
            leeches=leeches,
            author=author,
            category=category,
        ))

    return results


def parse_topic_page(html: str) -> TopicDetails:
    """Parse a topic page from viewtopic.php HTML."""
    soup = BeautifulSoup(html, "lxml")

    # Topic ID from link
    topic_id = 0
    link = soup.find("a", class_="mr-2") or soup.find("link", rel="canonical")
    if link:
        href = link.get("href", "")
        m = re.search(r"t=(\d+)", str(href))
        if m:
            topic_id = int(m.group(1))

    # Title
    title_tag = soup.find(id="topic-title")
    title = _text(title_tag)

    # Magnet link
    magnet_tag = soup.find("a", class_="magnet-link")
    magnet_link = str(magnet_tag["href"]) if magnet_tag and magnet_tag.get("href") else ""

    # Size — from download link or tor-size-humn span
    size = ""
    size_tag = soup.find("span", id="tor-size-humn")
    if size_tag:
        size = _text(size_tag)
    if not size:
        dl_link = soup.find("a", class_="dl-stub")
        if dl_link:
            size = _text(dl_link)

    # Seeds / leeches from the topic page seed/leech info
    seeds_tag = soup.find("span", class_="seed")
    seeds = _int(_text(seeds_tag.find("b"))) if seeds_tag else 0

    leeches_tag = soup.find("span", class_="leech")
    leeches = _int(_text(leeches_tag.find("b"))) if leeches_tag else 0

    # Description — first post body
    post_body = soup.find("div", class_="post_body")
    description = post_body.get_text(separator="\n", strip=True) if post_body else ""
    # Truncate very long descriptions
    if len(description) > 4000:
        description = description[:4000] + "\n...[truncated]"

    # File list — from spoiler with class "filelist" or similar
    file_list: list[str] = []
    filelist_div = soup.find("div", class_="sp-body", id=re.compile(r"filelist"))
    if not filelist_div:
        # Alternative: look for spoiler with "Список файлов" title
        for sp in soup.find_all("div", class_="sp-fold"):
            sp_title = sp.find("span", class_="sp-title")
            if sp_title and "файл" in _text(sp_title).lower():
                filelist_div = sp.find("div", class_="sp-body")
                break

    if filelist_div:
        for li in filelist_div.find_all("li"):
            file_list.append(li.get_text(strip=True))
        if not file_list:
            # Fall back to plain text split by newlines
            text = filelist_div.get_text(separator="\n", strip=True)
            file_list = [line for line in text.splitlines() if line.strip()]

    return TopicDetails(
        topic_id=topic_id,
        title=title,
        magnet_link=magnet_link,
        size=size,
        seeds=seeds,
        leeches=leeches,
        description=description,
        file_list=file_list,
    )

"""MkDocs hooks: publish repository Markdown files as site pages.

SECURITY.md has to live at the repository root (GitHub looks for it there, and the "Report a vulnerability" button
depends on it). It is also a page the site should carry, so it is added to the build from where it is rather than
copied into docs/ and left to drift. `extra.repo_pages` in mkdocs.yml maps each repository file to its page.
"""

from pathlib import Path

from mkdocs.config.defaults import MkDocsConfig
from mkdocs.structure.files import File, Files

ROOT = Path(__file__).parent


def on_files(files: Files, config: MkDocsConfig) -> Files:
    """Add the site's repository pages to the build without copying them into its docs directory."""
    for source, page in config.extra.get("repo_pages", {}).items():
        content = (ROOT / source).read_text(encoding="utf-8")
        files.append(File.generated(config, page, content=content))
    return files

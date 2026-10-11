"""Prints an HTML file as plain text: the Rust toolchain ships its standard
library's notices (COPYRIGHT-library.html) only as HTML."""

import html.parser
import sys


class Text(html.parser.HTMLParser):
    BLOCK = {"p", "div", "br", "li", "h1", "h2", "h3", "h4", "pre", "tr", "section"}

    def __init__(self):
        super().__init__()
        self.out = []
        self.skip = 0

    def handle_starttag(self, tag, attrs):
        if tag in ("style", "script", "head"):
            self.skip += 1
        elif tag in self.BLOCK:
            self.out.append("\n")
            if tag == "li":
                self.out.append("  - ")

    def handle_endtag(self, tag):
        if tag in ("style", "script", "head"):
            self.skip -= 1
        elif tag in self.BLOCK:
            self.out.append("\n")

    def handle_data(self, data):
        if not self.skip:
            self.out.append(data)


parser = Text()
with open(sys.argv[1], encoding="utf-8") as f:
    parser.feed(f.read())
lines = [line.rstrip() for line in "".join(parser.out).splitlines()]
# Collapse runs of blank lines.
text = "\n".join(line for i, line in enumerate(lines) if line or (i and lines[i - 1]))
print(text.strip())

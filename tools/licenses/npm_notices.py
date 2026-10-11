"""Prints the license notices of the web UI's runtime npm packages: every
non-dev package in package-lock.json, with its LICENSE file. plotly.js
bundles further libraries into one file and lists their notices in a
"Bundled license information" comment, which the Vite build strips, so that
block is printed too. Fails if a package has no license file to print."""

import glob
import json
import os
import sys

web = sys.argv[1]
with open(os.path.join(web, "package-lock.json"), encoding="utf-8") as f:
    lock = json.load(f)

missing = []
for path, info in sorted(lock["packages"].items()):
    if not path or info.get("dev"):
        continue
    name = path.split("node_modules/")[-1]
    pkg_dir = os.path.join(web, path)
    files = sorted(
        p for p in glob.glob(os.path.join(pkg_dir, "*"))
        if os.path.basename(p).upper().startswith(("LICENSE", "LICENCE", "COPYING"))
    )
    print("\n" + "-" * 80)
    print(f"{name} {info.get('version', '?')} ({info.get('license', '?')})\n")
    if not files:
        missing.append(name)
        continue
    for file in files:
        with open(file, encoding="utf-8") as f:
            print(f.read().strip())

    if name == "plotly.js-dist-min":
        with open(os.path.join(pkg_dir, "plotly.min.js"), encoding="utf-8") as f:
            js = f.read()
        start = js.find("Bundled license information:")
        if start < 0:
            sys.exit("plotly.min.js: no 'Bundled license information' block found")
        print("\nplotly.js bundles the following, with these notices:\n")
        print(js[start : js.find("*/", start)].strip())

if missing:
    sys.exit(f"no license file in: {', '.join(missing)}")

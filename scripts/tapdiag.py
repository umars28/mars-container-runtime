#!/usr/bin/env python3
import json
import re
import sys

text = sys.stdin.read()

for line in text.splitlines():
    if line.startswith(("ok ", "not ok ")):
        print(line)

seen = set()
for match in re.finditer(r'"(error|stderr|stdout)": (".*?[^\\]")', text, re.S):
    key = match.group(1)
    try:
        value = json.loads(match.group(2))
    except json.JSONDecodeError:
        continue
    if not value.strip():
        continue
    if key == "stdout":
        failures = [l for l in value.splitlines() if l.startswith("not ok")]
        for f in failures[:6]:
            if f not in seen:
                seen.add(f)
                print("    inner:", f)
    else:
        snippet = value.strip().replace("\n", " | ")[:400]
        if snippet not in seen:
            seen.add(snippet)
            print(f"    {key}: {snippet}")

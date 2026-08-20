#!/usr/bin/env python3
"""What the vector channel sees, per query, before anything is dropped.

Reads the engine's own debug log rather than its answer: the answer has already
been filtered, and the shape of the whole candidate field is what a cut-off has
to be derived from. This is how `VECTOR_RESCUE_FLOOR` was set, and how it should
be re-checked after a change to the model, the fusion or the floor itself.

    RUST_LOG is set by this script; build the engine first.
    python3 scripts/search-distribution.py /path/to/a/real/project

What the classes are for: `garbage` must not produce word matches, `off-topic`
is real language the corpus cannot answer, `other-tongue` is meaning the corpus
does hold asked in another language — it has to survive whatever the floor is —
and `semantic`/`lexical` are the queries the project answers.
"""

import json
import os
import re
import subprocess
import sys
import tempfile

ENGINE = os.environ.get(
    "MEMORY_HUB_BINARY",
    os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "target", "release", "memory-hub"),
)
REPO = sys.argv[1] if len(sys.argv) > 1 else "."

QUERIES = [
    # Real words, real meaning, nothing to do with this project. The class a
    # floor has to reject without rejecting a synonym it has never seen.
    ("off-topic", "banana bread recipe"),
    ("off-topic", "football transfer window"),
    ("off-topic", "mortgage interest rates"),
    ("off-topic", "photosynthesis"),
    # Meaning the corpus holds, said in another language: the model is
    # multilingual, and a floor tuned on English alone would cut this.
    ("other-tongue", "шифрование записей"),
    ("other-tongue", "расширения приложения"),
    ("garbage", "qqqqqqqqq"),
    ("garbage", "asdfghjkl"),
    ("garbage", "zzzzzzzzzz"),
    ("garbage", "xkcdvbnm qwerty"),
    ("semantic", "plugin"),
    ("semantic", "add-on"),
    ("semantic", "keyboard shortcut"),
    ("semantic", "colour scheme"),
    ("semantic", "vector database"),
    ("semantic", "how records are stored"),
    ("lexical", "extension"),
    ("lexical", "memory"),
    ("lexical", "search"),
]

log = tempfile.NamedTemporaryFile(suffix=".log", delete=False, mode="w+")
engine = subprocess.Popen(
    [ENGINE, "mcp"],
    cwd=REPO,
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=log,
    text=True,
    bufsize=1,
    env={**os.environ, "RUST_LOG": "memory_hub_index=debug"},
)

next_id = [0]


def request(method, params):
    next_id[0] += 1
    engine.stdin.write(
        json.dumps({"jsonrpc": "2.0", "id": next_id[0], "method": method, "params": params}) + "\n"
    )
    engine.stdin.flush()
    while True:
        line = engine.stdout.readline()
        if not line:
            raise SystemExit("engine closed")
        answer = json.loads(line)
        if answer.get("id") == next_id[0]:
            return answer


request(
    "initialize",
    {
        "protocolVersion": "2025-11-25",
        "capabilities": {},
        "clientInfo": {"name": "probe", "version": "0"},
        "_meta": {"memoryHub": {"memoryInterfaceVersion": {"major": 2, "minor": 0}}},
    },
)

returned = {}
for _, query in QUERIES:
    answer = request(
        "tools/call",
        {"name": "memory_search", "arguments": {"query": query, "limit": 10}},
    )
    payload = answer.get("result", {}).get("structuredContent", {}) or {}
    returned[query] = (payload.get("total"), payload.get("mode"))

engine.terminate()
engine.wait(timeout=20)
log.flush()
log.seek(0)
# The subscriber colours its output; the escapes would break every regex below.
lines = [re.sub(r"\x1b\[[0-9;]*m", "", line) for line in log.read().splitlines()]

fields = {}
for line in lines:
    if "vector rescue candidates" not in line:
        continue
    query = re.search(r"query=(.*?) candidates=", line)
    scores = re.search(r"scores=([\d\. ]+)", line)
    if query and scores:
        fields[query.group(1).strip()] = [float(v) for v in scores.group(1).split()]

print(f"{'class':9} {'query':22} {'total':>5} {'mode':7} top    2nd    median tail   n")
for label, query in QUERIES:
    scores = fields.get(query, [])
    total, mode = returned.get(query, (None, None))
    if not scores:
        print(f"{label:9} {query:22} {str(total):>5} {str(mode):7} (vector channel did not run)")
        continue
    ordered = sorted(scores, reverse=True)
    top = ordered[0]
    second = ordered[1] if len(ordered) > 1 else float("nan")
    median = ordered[len(ordered) // 2]
    tail = ordered[-1]
    print(
        f"{label:9} {query:22} {str(total):>5} {str(mode):7} "
        f"{top:.3f}  {second:.3f}  {median:.3f}  {tail:.3f}  {len(ordered)}"
    )

print(f"\nlog: {log.name}")

#!/usr/bin/env python3
"""What one search costs the caller, in bytes and in milliseconds.

A search answers which records to read. How much it sends while answering that
is a number, not an opinion, and it is the number that decides whether an agent
can afford to search twice — so it is measured here rather than argued about.

    cargo build --release -p memory-hub
    python3 scripts/search-answer-size.py /path/to/a/real/project "a real question"

Point it at two builds to compare them:

    python3 scripts/search-answer-size.py PROJECT "query" --binary ./old-memory-hub

Reports the size of the answer, the share of it that is text from the records,
the size per hit, and the best of five timings. `total` is printed beside the
page size because a count that equals `limit + 1` is a page being reported as a
corpus.
"""

import argparse
import json
import subprocess
import sys
import time

PROTOCOL = "2025-11-25"
INTERFACE = {"major": 1, "minor": 0}


def ask(binary, project, query, limit, records):
    """Run one search against a server started for this call, and answer it."""
    server = subprocess.Popen(
        [binary, "mcp", "--project", project, "--records", records],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )

    def send(message):
        server.stdin.write(json.dumps(message) + "\n")
        server.stdin.flush()

    def reply():
        while True:
            line = server.stdout.readline()
            if not line:
                raise SystemExit("the server closed without answering")
            message = json.loads(line)
            if "id" in message:
                return message

    send({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL,
            "capabilities": {},
            "clientInfo": {"name": "search-answer-size", "version": "1"},
            "_meta": {"memoryHub": {"memoryInterfaceVersion": INTERFACE}},
        },
    })
    reply()
    send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    timings = []
    answer = None
    for _ in range(5):
        started = time.perf_counter()
        send({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {
                "name": "memory_search",
                "arguments": {"query": query, "limit": limit},
            },
        })
        answer = reply()
        timings.append((time.perf_counter() - started) * 1000)

    server.stdin.close()
    server.wait(timeout=10)
    return answer, min(timings)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("project", help="a repository whose memory has records in it")
    parser.add_argument("query", help="a question somebody would really ask")
    parser.add_argument("--limit", type=int, default=20)
    parser.add_argument("--binary", default="target/release/memory-hub")
    parser.add_argument("--records", default="git-metadata")
    arguments = parser.parse_args()

    answer, elapsed = ask(
        arguments.binary, arguments.project, arguments.query,
        arguments.limit, arguments.records,
    )
    result = answer.get("result", {}).get("structuredContent")
    if result is None:
        print(json.dumps(answer)[:800], file=sys.stderr)
        raise SystemExit("the server answered with no search result")

    wire = len(json.dumps(result, ensure_ascii=False).encode("utf-8"))
    hits = result.get("hits", [])
    text = sum(
        len(json.dumps(hit.get("content") or hit.get("excerpt") or "",
                       ensure_ascii=False).encode("utf-8"))
        for hit in hits
    )
    capped = "+" if result.get("total_capped") else ""
    print(f"answer            {wire:>10,} bytes")
    print(f"  of which text   {text:>10,} bytes  ({text * 100 // max(wire, 1)}%)")
    print(f"  per hit         {wire // max(len(hits), 1):>10,} bytes")
    print(f"hits              {len(hits):>10}")
    print(f"total             {result.get('total'):>10}{capped}")
    print(f"time              {elapsed:>10.1f} ms  (best of 5)")


if __name__ == "__main__":
    main()

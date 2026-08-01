#!/usr/bin/env python3
"""Generate synthetic projects for the README screenshots.

The code view draws real source: names, paths, and call structure. Screenshots
taken against anything real would publish whatever happened to be on the
machine, so the documented images are produced from invented code instead.

Everything here is fictional. Regenerate, re-crawl into a throwaway database
and re-shoot whenever the interface changes:

    python3 tools/fixtures/demo_graph.py /tmp/demo
    export RUNAR_CODEGRAPH_PATH=/tmp/demo/codegraph.db
    runar crawl /tmp/demo/atlas-weather   -p atlas-weather
    runar crawl /tmp/demo/beacon-relay    -p beacon-relay
    runar crawl /tmp/demo/cinder-parser   -p cinder-parser
    runar graph serve --project atlas-weather
"""

import os
import random
import sys

# Deterministic: the same fixture every run, so a re-shot screenshot differs
# only where the interface actually changed.
RNG = random.Random(20260801)

MODULES = {
    "ingest": ["station_reader", "packet_decoder", "sample_buffer", "feed_poller"],
    "pipeline": ["normalise", "aggregate", "windowing", "calibrate", "dedupe"],
    "storage": ["column_store", "index_writer", "snapshot", "retention"],
    "api": ["routes", "handlers", "serializer"],
    "model": ["reading", "station", "forecast"],
    "util": ["clock", "units", "checksum"],
}

VERBS = ["build", "parse", "resolve", "collect", "apply", "merge", "emit",
         "validate", "compact", "estimate", "align", "scan", "flush", "derive"]
NOUNS = ["reading", "window", "segment", "packet", "station", "batch", "sample",
         "channel", "profile", "bucket", "cursor", "envelope", "series"]


def fn_name():
    return f"{RNG.choice(VERBS)}_{RNG.choice(NOUNS)}"


def body(branches, calls):
    """A function body with a known branch count, so complexity is controlled."""
    lines = []
    for i in range(branches):
        lines.append(f"  if (input.channel === {i} || input.depth > {i * 3}) {{")
        lines.append(f"    total += {i + 1};")
        lines.append("  }")
    for target, mod in calls:
        lines.append(f"  total += {target}(input);")
    lines.append("  return total;")
    return "\n".join(lines)


def make_project(root, name, files_per_module, seed_bias, fns_per_file=9):
    """One fictional project. `seed_bias` shifts how gnarly its worst code is."""
    api = {}          # module -> [(fn, file)]
    for mod, stems in MODULES.items():
        for stem in stems[: files_per_module]:
            path = f"src/{mod}/{stem}.ts"
            seen = set()
            while len(seen) < fns_per_file:
                seen.add(fn_name())
            for fn in sorted(seen):
                api.setdefault(mod, []).append((fn, path))

    flat = [(fn, path, mod) for mod, entries in api.items() for fn, path in entries]

    for mod, entries in api.items():
        by_file = {}
        for fn, path in entries:
            by_file.setdefault(path, []).append(fn)

        for path, fns in by_file.items():
            # Call a handful of functions from other modules, so the graph has
            # cross-district edges for the arcs and the egonet to show.
            targets = [t for t in RNG.sample(flat, min(4, len(flat)))
                       if t[1] != path]
            imports = {}
            for fn, tpath, tmod in targets:
                rel = "../" + tpath[len("src/"):].rsplit(".ts", 1)[0]
                imports.setdefault(rel, []).append(fn)

            out = ["// Fictional source, generated for documentation screenshots.",
                   "// Nothing here corresponds to a real project."]
            for rel, names in imports.items():
                out.append(f'import {{ {", ".join(sorted(set(names)))} }} from "{rel}";')
            out.append("")
            out.append("interface Input { channel: number; depth: number; }")
            out.append("")

            for i, fn in enumerate(fns):
                # One outlier per project so the city has a landmark tower, the
                # rest spread out so the sqrt height scale has something to show.
                if i == 0 and path.endswith(("routes.ts", "normalise.ts")):
                    branches = seed_bias + RNG.randint(18, 26)
                else:
                    branches = RNG.choice([0, 1, 1, 2, 2, 3, 4, 6, 9])
                picked = [(t[0], t[2]) for t in targets[: RNG.randint(0, 3)]]
                out.append(f"export function {fn}(input: Input): number {{")
                out.append("  let total = 0;")
                out.append(body(branches, picked))
                out.append("}")
                out.append("")

            full = os.path.join(root, name, path)
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, "w") as fh:
                fh.write("\n".join(out))

    with open(os.path.join(root, name, "package.json"), "w") as fh:
        fh.write(f'{{"name": "{name}", "version": "1.0.0", "private": true}}\n')


def main():
    root = sys.argv[1] if len(sys.argv) > 1 else "/tmp/demo"
    os.makedirs(root, exist_ok=True)
    # Three, so the project switcher has something real to switch between.
    make_project(root, "atlas-weather", files_per_module=4, seed_bias=8)
    make_project(root, "beacon-relay", files_per_module=2, seed_bias=2)
    make_project(root, "cinder-parser", files_per_module=3, seed_bias=4)
    print(f"wrote fixtures under {root}")
    for p in ("atlas-weather", "beacon-relay", "cinder-parser"):
        n = sum(len(fs) for _, _, fs in os.walk(os.path.join(root, p)))
        print(f"  {p}: {n} files")


if __name__ == "__main__":
    main()

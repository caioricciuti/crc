#!/usr/bin/env python3
"""Build-free supervisor for isolated native self-test lanes (stdlib only)."""

import argparse
import os
from pathlib import Path
import random
import re
import shutil
import signal
import subprocess
import tempfile
import time


def assign(scenarios, jobs, shuffle=False, weights=None):
    """Longest estimated scenarios first, into the least loaded lane."""
    lanes = [[] for _ in range(min(jobs, len(scenarios)))]
    loads = [0.0] * len(lanes)
    weighted = []
    for path in scenarios:
        text = path.read_text()
        weight = sum(float(n) for n in re.findall(r"\bwait (\d+(?:\.\d+)?)", text))
        weight += 3 * text.count("CRC_SELFTEST=")
        weight = (weights or {}).get(path.stem[3:], max(weight, 1))
        weighted.append((weight, path))
    for weight, path in sorted(weighted, key=lambda pair: (-pair[0], pair[1].name)):
        lane = min(range(len(lanes)), key=lambda i: (loads[i], i))
        lanes[lane].append(path)
        loads[lane] += weight
    if shuffle:
        rng = random.Random(0)
        for lane in lanes:
            rng.shuffle(lane)
    return lanes


def stop(workers):
    # Each lane owns a process group; never signal the installed app or
    # another run. Hold helpers may detach and are cleaned by the shell.
    for process in workers:
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
    for process in workers:
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jobs", type=int, required=True)
    parser.add_argument("--debug", choices=("0", "1"), default="0")
    parser.add_argument("--shuffle", choices=("0", "1"), default="0")
    parser.add_argument("--build-secs", required=True)
    parser.add_argument("filters", nargs="*")
    args = parser.parse_args()
    if args.jobs < 1:
        parser.error("--jobs must be positive")
    scenarios = [p for p in sorted(Path("scripts/selftest").glob("[0-9][0-9]-*.sh"))
                 if not args.filters or any(word in p.stem for word in args.filters)]
    if not scenarios:
        print("selftest: no scenario matches: " + " ".join(args.filters), flush=True)
        return 1
    started = time.time()
    since = time.strftime("%Y-%m-%d %H:%M:%S")
    root = Path(tempfile.mkdtemp(prefix="crc-selftest-"))
    workers = []
    logs = []
    failed = False
    print(f"selftest lanes: {root}", flush=True)

    def interrupt(signum, frame):
        raise KeyboardInterrupt

    previous = {sig: signal.signal(sig, interrupt) for sig in (signal.SIGINT, signal.SIGTERM)}
    try:
        weights = {}
        for line in Path("scripts/selftest.weights").read_text().splitlines():
            if line and not line.startswith("#"):
                name, seconds = line.split()
                weights[name] = float(seconds)
        lanes = assign(scenarios, args.jobs, args.shuffle == "1", weights)
        for index, scenarios_in_lane in enumerate(lanes, 1):
            lane = root / f"lane-{index}"
            lane.mkdir()
            (lane / "scenarios.list").write_text("".join(p.stem + "\n" for p in scenarios_in_lane))
            log = (root / f"lane-{index}.log").open("w")
            logs.append(log)
            command = ["bash", "scripts/selftest.sh", "--lane", str(lane)]
            if args.debug == "1":
                command.append("--debug")
            workers.append(subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT,
                                            start_new_session=True))
            print(f"lane {index}: {len(scenarios_in_lane)} scenarios", flush=True)
        # Poll every lane so a short lane reports completion immediately,
        # even while the first lane still runs its longest scenario.
        pending = set(range(len(workers)))
        while pending:
            for index in sorted(pending):
                result = workers[index].poll()
                if result is None:
                    continue
                pending.remove(index)
                logs[index].close()
                lane = root / f"lane-{index + 1}"
                print(f"lane {index + 1}: {'pass' if result == 0 else f'FAIL (exit {result})'}", flush=True)
                failed |= result != 0
            if pending:
                time.sleep(0.2)
    except KeyboardInterrupt:
        failed = True
        print("selftest: interrupted", flush=True)
        return 130
    except Exception:
        failed = True
        raise
    finally:
        stop(workers)
        for log in logs:
            log.close()
        for sig, handler in previous.items():
            signal.signal(sig, handler)
        with (root / "selftest.log").open("w") as combined:
            for path in sorted(root.glob("lane-*.log")):
                combined.write(f"=== {path.stem} ===\n")
                combined.write(path.read_text())
        if failed:
            print(f"Self-test failure artifacts: {root}", flush=True)
    if failed:
        for index, process in enumerate(workers, 1):
            if process.returncode:
                print(f"=== lane {index} failure (last 30 lines) ===", flush=True)
                print("\n".join((root / f"lane-{index}.log").read_text().splitlines()[-30:]), flush=True)
        sleep_log = subprocess.run(["pmset", "-g", "log"], capture_output=True, text=True)
        if any(line[:19] >= since and " Sleep " in line for line in sleep_log.stdout.splitlines()):
            print("Note: the Mac went to sleep during this run; run again with it awake.", flush=True)
    print(f"timing: build {args.build_secs}", flush=True)
    print(f"timing: parallel {time.time() - started:.1f}", flush=True)
    if not failed:
        print(f"selftest: {len(scenarios)} scenarios in {len(workers)} lanes pass", flush=True)
        shutil.rmtree(root)
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())

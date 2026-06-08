#!/usr/bin/env python3
"""Run the 6tsou SETNX golden transcript against oracle/candidate/baseline."""

import argparse
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def wait_for_port(port: int, timeout_s: float = 5.0) -> None:
    deadline = time.monotonic() + timeout_s
    last_error: OSError | None = None
    while time.monotonic() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError as exc:
            last_error = exc
            time.sleep(0.02)
    raise RuntimeError(f"server did not open port {port}: {last_error}")


def start_frankenredis(binary: str, port: int, cwd: Path) -> subprocess.Popen[str]:
    return subprocess.Popen(
        [binary, "--bind", "127.0.0.1", "--port", str(port)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        cwd=cwd,
        text=True,
    )


def start_oracle(binary: str, port: int, cwd: Path) -> subprocess.Popen[str]:
    return subprocess.Popen(
        [
            binary,
            "--bind",
            "127.0.0.1",
            "--port",
            str(port),
            "--save",
            "",
            "--appendonly",
            "no",
            "--protected-mode",
            "no",
            "--dir",
            str(cwd),
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        cwd=cwd,
        text=True,
    )


def stop(process: subprocess.Popen[str]) -> None:
    process.terminate()
    try:
        process.wait(timeout=3.0)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=3.0)
    if process.returncode not in (0, -15) and process.stderr is not None:
        err = process.stderr.read()
        if err:
            print(err, file=sys.stderr)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--oracle-bin", required=True)
    parser.add_argument("--candidate-bin", required=True)
    parser.add_argument("--baseline-bin", required=True)
    parser.add_argument("--oracle-port", type=int, default=16560)
    parser.add_argument("--candidate-port", type=int, default=16561)
    parser.add_argument("--baseline-port", type=int, default=16562)
    args = parser.parse_args()

    here = Path(__file__).resolve().parent
    processes: list[subprocess.Popen[str]] = []
    with tempfile.TemporaryDirectory(prefix="fr-6tsou-oracle-") as oracle_dir:
        with tempfile.TemporaryDirectory(prefix="fr-6tsou-candidate-") as candidate_dir:
            with tempfile.TemporaryDirectory(prefix="fr-6tsou-baseline-") as baseline_dir:
                processes.append(start_oracle(args.oracle_bin, args.oracle_port, Path(oracle_dir)))
                processes.append(
                    start_frankenredis(args.candidate_bin, args.candidate_port, Path(candidate_dir))
                )
                processes.append(
                    start_frankenredis(args.baseline_bin, args.baseline_port, Path(baseline_dir))
                )
                try:
                    for port in (args.oracle_port, args.candidate_port, args.baseline_port):
                        wait_for_port(port)
                    return subprocess.run(
                        [
                            sys.executable,
                            str(here / "setnx_fastpath_golden.py"),
                            str(args.oracle_port),
                            str(args.candidate_port),
                            str(args.baseline_port),
                        ],
                        check=False,
                        text=True,
                    ).returncode
                finally:
                    for process in reversed(processes):
                        stop(process)


if __name__ == "__main__":
    raise SystemExit(main())

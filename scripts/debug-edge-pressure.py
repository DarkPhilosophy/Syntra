#!/usr/bin/env python3
"""Read-only edge-pressure telemetry from the daemon journal and glow trace."""
import argparse
import json
import os
from pathlib import Path
import re
import select
import subprocess
import time


class PressureDebug:
    def __init__(self):
        self.edge = None
        self.threshold = None
        self.last_motion = None
        self.phase = "WAITING_FOR_EDGE"

    def journal(self, message):
        armed = re.search(r"edge pressure armed at the (Left|Right|Top|Bottom) edge \(needs ([0-9.]+)\)", message)
        if armed:
            self.edge, threshold = armed.groups()
            self.threshold = float(threshold)
            self.last_motion = None
            self.phase = "ARMED"
            return f"ARMED threshold={self.threshold:g}"
        for needle, phase in (
            ("cannot accept remote input", "BLOCKED_PEER_NOT_READY"),
            ("ignoring the edge the pointer just entered through", "BLOCKED_ENTRY_GUARD"),
            ("edge pressure reached", "THRESHOLD_REACHED"),
            ("edge pressure released: the pointer moved back", "RESET_INWARD_MOTION"),
            ("edge pressure restarted from zero", "RESET_NEW_TOUCH"),
            ("edge push dropped", "RESET_CONTROLLER_CHANGED"),
            ("peer did not acknowledge entry", "ENTRY_ACK_TIMEOUT"),
            ("acknowledged entry", "ENTRY_ACKNOWLEDGED"),
            ("releasing input capture", "CAPTURE_RELEASED"),
        ):
            if needle in message:
                self.phase = phase
                return phase
        if "activated:" in message:
            return "PORTAL_ACTIVATED"
        motion = re.search(r"Motion \{ time: (\d+), dx: (-?[\d.eE+]+), dy: (-?[\d.eE+]+)", message)
        if self.phase != "ARMED" or not motion or self.edge is None:
            return None
        stamp, dx, dy = motion.groups()
        current = (int(stamp), float(dx), float(dy))
        if current == self.last_motion:
            return None  # One backend event can be logged for multiple edge handles.
        self.last_motion = current
        _, dx, dy = current
        push = {"Left": -dx, "Right": dx, "Top": -dy, "Bottom": dy}[self.edge]
        return f"MOTION outward_delta={push:+g} dx={dx:+g} dy={dy:+g}"

    def glow(self, line):
        match = re.search(r"Pressure \{ edge: (Left|Right|Top|Bottom).*amount: ([0-9.eE+-]+)", line)
        if not match:
            return None
        edge, value = match.groups()
        amount = float(value)
        measured = ""
        if edge == self.edge and self.threshold is not None:
            measured = f" pressure≈{amount * self.threshold:.2f}/{self.threshold:g}"
        return f"GLOW_RECEIVED level={amount * 100:.1f}%{measured}"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seconds", type=float, default=60, help="Observation duration; no settings are changed")
    args = parser.parse_args()
    if not 0 < args.seconds <= 3600:
        parser.error("--seconds must be between 0 and 3600")
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    if not runtime:
        parser.error("Run inside the graphical user's session (XDG_RUNTIME_DIR is missing)")
    trace_path = Path(runtime) / "syntra" / "edge-glow.log"
    decoder = PressureDebug()
    child = subprocess.Popen(
        ["journalctl", "--no-pager", "--follow", "--lines=0", "_SYSTEMD_USER_UNIT=syntra.service", "-o", "json"],
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
    )
    start = time.monotonic()
    pending = b""
    offset = trace_path.stat().st_size if trace_path.exists() else 0
    trace_pending = ""

    def emit(value):
        if value:
            print(f"{time.monotonic() - start:7.3f}s {value}", flush=True)

    try:
        emit("WAITING_FOR_EDGE (motion delta is NOT a separate velocity term; glow values are clipped telemetry)")
        while time.monotonic() - start < args.seconds:
            ready, _, _ = select.select([child.stdout], [], [], 0.04)
            if ready:
                data = os.read(child.stdout.fileno(), 65536)
                if not data:
                    raise RuntimeError("The journal observer stopped")
                pending += data
                while b"\n" in pending:
                    line, pending = pending.split(b"\n", 1)
                    try:
                        message = json.loads(line).get("MESSAGE", "")
                    except (ValueError, AttributeError):
                        continue
                    if isinstance(message, str):
                        emit(decoder.journal(message))
            try:
                with trace_path.open(encoding="utf-8") as trace:
                    if trace_path.stat().st_size < offset:
                        offset, trace_pending = 0, ""
                    trace.seek(offset)
                    trace_pending += trace.read()
                    offset = trace.tell()
                while "\n" in trace_pending:
                    line, trace_pending = trace_pending.split("\n", 1)
                    emit(decoder.glow(line))
            except FileNotFoundError:
                pass
        emit("OBSERVATION_FINISHED")
    except KeyboardInterrupt:
        pass
    finally:
        child.terminate()
        try:
            child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Build with wifi.env's provisioning, flash, then show the serial log.

    python3 scripts/flash_and_monitor.py [seconds]   (default 15)

Variables set in the shell win over wifi.env, so one wifi.env serves several
boards; INKPLATE_PORT picks the serial port (default /dev/ttyUSB0):

    INKPLATE_PORT=/dev/ttyUSB1 INKPLATE_SERVER_URL=<url> \\
        python3 scripts/flash_and_monitor.py

The board is reset as monitoring starts, so the log begins at boot. Under
WSL, a missing port is first attached with `usbipd attach`: the board must
have been bound once (`usbipd bind`, admin). INKPLATE_BUSID picks it when
several are bound.
"""

import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

PORT = os.environ.get("INKPLATE_PORT", "/dev/ttyUSB0")
BAUD = 115200
BINARY = "target/xtensa-esp32-none-elf/release/inkplate-dashboard"
CH340_VID_PID = "1a86:7523"


def usbipd_attach():
    """WSL only: attach the bound, unattached CH340. True once PORT exists."""
    if not shutil.which("powershell.exe"):
        return False
    busid = os.environ.get("INKPLATE_BUSID")
    if not busid:
        listing = subprocess.run(
            ["powershell.exe", "-NoProfile", "-Command", "usbipd list"],
            capture_output=True, text=True).stdout
        shared = [line.split()[0] for line in listing.splitlines()
                  if CH340_VID_PID in line and line.rstrip().endswith("Shared")]
        if len(shared) != 1:
            if len(shared) > 1:
                print(f"[!] Several bound CH340s ({', '.join(shared)}); set INKPLATE_BUSID")
            return False
        busid = shared[0]
    print(f"[*] usbipd attach --wsl --busid {busid}")
    subprocess.run(
        ["powershell.exe", "-NoProfile", "-Command",
         f"usbipd attach --wsl --busid {busid}"],
        capture_output=True, text=True)
    for _ in range(20):
        if Path(PORT).exists():
            return True
        time.sleep(0.5)
    return False


def build():
    env_file = Path(__file__).resolve().parent.parent / "wifi.env"
    if env_file.exists():
        for line in env_file.read_text().splitlines():
            line = line.strip()
            if line and not line.startswith("#") and "=" in line:
                key, _, val = line.partition("=")
                os.environ.setdefault(key.strip(), val.strip())
        print(f"[*] Loaded provisioning from {env_file.name}")
    else:
        print("[!] wifi.env missing: building unprovisioned (NO WIFI CONFIG screen)")
    print("[*] Building (cargo build --release)...")
    if subprocess.run(["cargo", "build", "--release"]).returncode != 0:
        sys.exit("[!] Build failed")
    print("[+] Build complete\n")


def monitor(duration):
    import serial

    print(f"[*] Monitoring for {duration}s...")
    print("-" * 50)
    ser = serial.Serial(PORT, BAUD, timeout=0.1)
    # Pulse EN via RTS so the firmware restarts at t=0 of the window.
    ser.dtr = False
    ser.rts = True
    time.sleep(0.1)
    ser.rts = False
    start = time.time()
    while (time.time() - start) < duration:
        try:
            line = ser.readline()
        except serial.SerialException:
            # usbip links occasionally drop: reopen without resetting the
            # board, and say so, since the gap is the link, not the firmware.
            print(f"[!] serial link dropped at {time.time() - start:.0f}s, reopening")
            ser.close()
            while (time.time() - start) < duration:
                time.sleep(1)
                if not Path(PORT).exists():
                    usbipd_attach()
                try:
                    ser = serial.Serial()
                    ser.port, ser.baudrate, ser.timeout = PORT, BAUD, 0.1
                    ser.dtr = ser.rts = False
                    ser.open()
                    break
                except serial.SerialException:
                    continue
            continue
        if line:
            elapsed = time.time() - start
            print(f"[{elapsed:6.1f}] " + line.decode("utf-8", errors="replace"), end="")
    ser.close()
    print("-" * 50)


def main():
    # Line-buffered even when redirected, so a log can be read as it grows.
    sys.stdout.reconfigure(line_buffering=True)
    duration = float(sys.argv[1]) if len(sys.argv) > 1 else 15.0
    build()
    if not (Path(PORT).exists() or usbipd_attach()):
        sys.exit(f"[!] {PORT} not found (set INKPLATE_PORT)")
    print(f"[*] Flashing {BINARY} via {PORT}...")
    # Never espflash's own monitor: it needs an interactive terminal.
    result = subprocess.run(
        ["espflash", "flash", "--non-interactive", "--port", PORT, BINARY])
    if result.returncode != 0:
        sys.exit(1)
    print("[+] Flash complete\n")
    time.sleep(0.3)
    monitor(duration)
    print("[*] Done")


if __name__ == "__main__":
    main()

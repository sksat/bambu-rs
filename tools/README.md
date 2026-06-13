# tools/ — clean-room device-observation scripts

Read-only Python scripts that talk to a real printer over raw MQTT-over-TLS to
**observe** the protocol (capture snapshots/deltas for fixtures and analysis).

They deliberately use the **Python standard library only** (no third-party
MQTT/Bambu packages): hand-rolling the MQTT framing keeps this *direct device
observation* rather than a reference implementation, which the clean-room rule
requires. There are no dependencies to install — run with `python3 tools/x.py`
or `uv run tools/x.py` (each carries a PEP 723 `dependencies = []` header).

Environment: `BAMBU_IP`, `BAMBU_SERIAL`, `BAMBU_CODE` (the access code is read
from the environment and **never printed**).

| script | what it captures |
| --- | --- |
| `capture_pushall.py` | the full `pushall` snapshot (→ `tests/fixtures/`) |
| `capture_get_version.py` | the `info.get_version` module inventory |
| `capture_report_delta.py` | the snapshot **plus** every delta over a window — run it, then trigger a change (AMS load/unload, a setting) to observe its wire format |

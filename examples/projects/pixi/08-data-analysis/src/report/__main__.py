"""`python -m report --config /etc/report/report.toml`."""

import argparse
import tomllib
from pathlib import Path

from report.summarize import summarize


def main() -> None:
    """Run the report the config describes."""
    parser = argparse.ArgumentParser(prog="report")
    parser.add_argument("--config", type=Path, default=Path("/etc/report/report.toml"))
    args = parser.parse_args()
    config = tomllib.loads(args.config.read_text())
    summary = summarize(Path(config["input"]), config["group_by"])
    output = Path(config["output"])
    output.parent.mkdir(parents=True, exist_ok=True)
    summary.write_csv(output)
    print(summary)
    print(f"wrote {output}")


if __name__ == "__main__":
    main()

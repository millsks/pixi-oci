"""The summary itself, with polars."""

from pathlib import Path

import polars as pl


def summarize(path: Path, group_by: str) -> pl.DataFrame:
    """Revenue and units per `group_by`, largest revenue first."""
    sales = pl.read_csv(path)
    return (
        sales.with_columns((pl.col("units") * pl.col("price")).alias("revenue"))
        .group_by(group_by)
        .agg(pl.col("units").sum(), pl.col("revenue").sum())
        .sort("revenue", descending=True)
    )

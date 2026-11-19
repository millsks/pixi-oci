"""Train a gradient-boosted classifier on the bundled data and save it where the container may write.

The code in /app is owned by root and read-only, and the image runs as --user 1000:1000, so the model goes to
/tmp, or to a mounted directory: `train.py --out /out/model.json`.
"""

import argparse
from pathlib import Path

import pandas as pd
import xgboost as xgb
from sklearn.model_selection import train_test_split


def main() -> None:
    """Train, report accuracy, save the model."""
    parser = argparse.ArgumentParser()
    parser.add_argument("--data", type=Path, default=Path("/app/data/flowers.csv"))
    parser.add_argument("--out", type=Path, default=Path("/tmp/model.json"))
    args = parser.parse_args()

    frame = pd.read_csv(args.data)
    features = frame.drop(columns="species")
    labels = frame["species"].astype("category")
    x_train, x_test, y_train, y_test = train_test_split(
        features, labels.cat.codes, test_size=0.25, random_state=0, stratify=labels.cat.codes
    )
    model = xgb.XGBClassifier(n_estimators=50, max_depth=3, random_state=0)
    model.fit(x_train, y_train)
    print(f"accuracy {model.score(x_test, y_test):.2f} on {len(x_test)} held-out rows")
    args.out.parent.mkdir(parents=True, exist_ok=True)
    model.save_model(args.out)
    print(f"wrote {args.out}")


if __name__ == "__main__":
    main()

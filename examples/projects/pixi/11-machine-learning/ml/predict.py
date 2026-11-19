"""Predict with a model train.py saved: `predict.py --model /tmp/model.json 5.1 3.5 1.4 0.2`."""

import argparse
from pathlib import Path

import xgboost as xgb


def main() -> None:
    """Load the model and classify one flower."""
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", type=Path, default=Path("/tmp/model.json"))
    parser.add_argument("measurements", type=float, nargs=4, metavar="CM")
    args = parser.parse_args()
    model = xgb.XGBClassifier()
    model.load_model(args.model)
    print(f"class {int(model.predict([args.measurements])[0])}")


if __name__ == "__main__":
    main()

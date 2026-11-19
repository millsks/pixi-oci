"""A small FastAPI service, added to the image with --add app:/app."""

from fastapi import FastAPI

app = FastAPI()


@app.get("/")
def hello() -> dict[str, str]:
    """Say hello."""
    return {"hello": "from pixi-oci"}

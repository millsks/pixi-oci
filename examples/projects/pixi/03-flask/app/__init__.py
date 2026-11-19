"""A Flask application served by gunicorn: `gunicorn 'app:create_app()'` from /srv."""

from flask import Flask, render_template


def create_app() -> Flask:
    """Make the application."""
    app = Flask(__name__)

    @app.get("/")
    def index() -> str:
        return render_template("index.html", title="pixi-oci")

    @app.get("/health")
    def health() -> dict[str, str]:
        return {"status": "ok"}

    return app

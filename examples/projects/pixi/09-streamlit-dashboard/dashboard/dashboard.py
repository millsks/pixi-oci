"""A Streamlit dashboard: `streamlit run dashboard.py` from /app, which also holds .streamlit/config.toml."""

import altair as alt
import pandas as pd
import streamlit as st

st.title("Weekly signups")
data = pd.DataFrame(
    {
        "week": pd.date_range("2026-01-05", periods=8, freq="W-MON"),
        "signups": [12, 18, 15, 22, 30, 27, 35, 41],
    }
)
st.altair_chart(alt.Chart(data).mark_line(point=True).encode(x="week:T", y="signups:Q"))
st.dataframe(data, hide_index=True)

# Summarise sales by region and plot them. The code is added to /app and the data to /data, as separate --adds.
suppressPackageStartupMessages({
  library(dplyr)
  library(ggplot2)
  library(readr)
})

args <- commandArgs(trailingOnly = TRUE)
input <- if (length(args) >= 1) args[[1]] else "/data/sales.csv"
output <- if (length(args) >= 2) args[[2]] else "/tmp/sales.png"

sales <- read_csv(input, show_col_types = FALSE)
by_region <- sales |>
  group_by(region) |>
  summarise(revenue = sum(units * price), units = sum(units), .groups = "drop") |>
  arrange(desc(revenue))
print(by_region)

plot <- ggplot(by_region, aes(x = region, y = revenue)) + geom_col()
ggsave(output, plot, width = 6, height = 4, dpi = 100)
cat("wrote", output, "\n")

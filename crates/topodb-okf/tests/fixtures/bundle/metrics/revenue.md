---
type: metric
title: Revenue
description: Monthly recurring revenue across all plans.
resource: dashboards/revenue
tags:
  - finance
  - kpi
status: stable
stale_after: "2026-12-31"
generated:
  by: openwiki/0.3
  at: "2026-08-01T00:00:00Z"
verified:
  - by: human:drew
    at: "2026-08-02T00:00:00Z"
sources:
  - resource: https://example.com/report.pdf
    id: rep-1
    title: Q3 Finance Report
    author: analyst/1.0
    usage_count: 4
    last_modified: "2026-07-30T00:00:00Z"
runtime: python3.11
custom_field:
  nested_key: nested_value
  depth:
    deeper: 7
---
Revenue is tracked monthly. See [Customers](/tables/customers.md) for the
underlying records and the [missing page](/tables/ghost.md) that is not in
this bundle.

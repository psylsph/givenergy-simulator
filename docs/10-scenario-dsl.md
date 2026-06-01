# Scenario DSL

YAML

08:00:
  solar: 3500

09:00:
  fault: grid_loss

Assertions:

10:00:
  expect:
    soc_gt: 50

Used for CI regression tests.

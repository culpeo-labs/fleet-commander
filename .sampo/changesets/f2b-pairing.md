---
cargo/fleet-commander: minor
---

Add cross-workspace pairing state and connect UX. New `:connect`,
`:disconnect`, and `:connections` commands let you link two workspace agents
into an undirected pair. Pairings are persisted globally to
`~/.config/fleet-commander/pairings.yaml` and gate the upcoming
cross-workspace messaging tools.

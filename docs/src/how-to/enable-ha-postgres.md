# Enable HA with PostgreSQL

*Status: written in Phase 2, alongside the PostgreSQL backend.*

`alertthread` defaults to SQLite, which is exactly one replica by design. Switching the
state store to PostgreSQL is what allows more than one.

This guide will cover:

- Pointing `STATE_BACKEND=postgres` at an existing PostgreSQL (for example a CloudNativePG
  cluster).
- Running the PostgreSQL migrations.
- Scaling the Deployment past one replica, and the `RollingUpdate` strategy that becomes
  available once the RWO PVC is gone.
- Setting `slack.rate_limit_divisor` to the replica count, because each replica holds its
  own token bucket.

⚠️ If SQLite is configured and more than one replica is detected, the process **refuses to
start**. That is deliberate: silently corrupting correlation state because somebody scaled a
Deployment is not an acceptable failure mode for an alerting component.

Background on the trade-off is in [ADR 001 D4](../adr/001-adr.md).

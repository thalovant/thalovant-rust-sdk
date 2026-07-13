# Changelog

## 0.2.17

- Use the native TLS backend for MQTT so the published dependency graph no longer includes vulnerable `rustls-webpki 0.102.8`.
- Add an explicit regression assertion for the MQTT TLS backend and align runtime user-agent versions.
- Give CI and release-guard workflows explicit read-only repository permissions.

## 0.2.16

- Add typed `OperationResource` and `ControlPlane::get_operation` support.

# Communication Protocol

Canonical spec: [mcub-c/docs/PROTOCOL.md](../../mcub-c/docs/PROTOCOL.md) (MCUB v2.2.0).

mcub-rust implements the same wire protocol — binary spectrum `0xCA` sync + JSON metadata,
115200 8N1, identify handshake. No rust-specific deviations.

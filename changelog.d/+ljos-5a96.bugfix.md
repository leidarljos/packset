`packset` and `packset-embed` now build with rustls 0.23.46, rustls-webpki 0.103.16 and webpki-roots 1.0.11, which fix RUSTSEC-2026-0346 to 0349, 0381 and 0382.
`packset-embed` also takes webpki-root-certs 1.0.11 (RUSTSEC-2026-0383).
`packset` builds with tokio 1.54.1 (RUSTSEC-2026-0386), and every crate with pulldown-cmark 0.13.6 (RUSTSEC-2026-0414 and 0433).
The writer's Lightning Memory-Mapped Database (LMDB) layer no longer pulls in the unmaintained bincode.

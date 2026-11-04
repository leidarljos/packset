A release tag builds packset, packsetd, packset-mcp and packset-embed.
Hosts are aarch64-apple-darwin and x86_64-apple-darwin.
Linux hosts are aarch64-unknown-linux-gnu and x86_64-unknown-linux-gnu.
The archive is `packset-v<version>-<target>.tar.gz`.
A `.sha256` file sits beside it.
`cargo binstall packset` downloads that archive.
The release publishes the tag only after CI succeeded on that commit.

# Root store

The ECDSA trust anchors the firmware verifies servers against, in the order
`src/roots.rs` tries them. `roots::tests` (run by the host suite) pins each
file's SHA-256.

| File | Certificate | Source |
|---|---|---|
| `gts_root_r4.der` | GTS Root R4 | Ubuntu `ca-certificates` |
| `gts_root_r3.der` | GTS Root R3 | Ubuntu `ca-certificates` |
| `isrg_root_ye.der` | ISRG Root YE (Let's Encrypt Generation Y) | letsencrypt.org/certs/gen-y/root-ye.der, checked against its self-signature, the X2-signed Root YE in the live chain (same key) and CT logs |
| `isrg_root_x2.der` | ISRG Root X2 | Ubuntu `ca-certificates` |
| `sslcom_tls_ecc_2022.der` | SSL.com TLS ECC Root CA 2022 | Ubuntu `ca-certificates` |

A root is tried only where some certificate in the served chain names it as
issuer (the chain is cut below that certificate), so the order matters only
where more than one root could anchor a chain. Root YE comes before X2: it
anchors Let's Encrypt's Generation Y one hop below X2's cross-sign, which
saves a P-384 verification.

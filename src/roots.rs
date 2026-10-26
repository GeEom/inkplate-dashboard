//! The ECDSA trust anchors, in the order tried; provenance in `roots/README.md`.
#![forbid(unsafe_code)]

pub const ROOTS: &[&[u8]] = &[
    include_bytes!("roots/gts_root_r4.der"),
    include_bytes!("roots/gts_root_r3.der"),
    include_bytes!("roots/isrg_root_ye.der"),
    include_bytes!("roots/isrg_root_x2.der"),
    include_bytes!("roots/sslcom_tls_ecc_2022.der"),
];

#[cfg(test)]
mod tests {
    use core::fmt::Write as _;
    use sha2::{Digest as _, Sha256};

    const SHA256: [&str; 5] = [
        "349dfa4058c5e263123b398ae795573c4e1313c83fe68f93556cd5e8031b3c7d",
        "34d8a73ee208d9bcdb0d956520934b4e40e69482596e8b6f73c8426b010a6f48",
        "e14ffcad5b0025731006caa43a121a22d8e9700f4fb9cf852f02a708aa5d5666",
        "69729b8e15a86efc177a57afb7171dfc64add28c2fca8cf1507e34453ccb1470",
        "c32ffd9f46f936d16c3673990959434b9ad60aafbb9e7cf33654f144cc1ba143",
    ];

    #[test]
    fn store_matches_pinned_hashes() {
        let hashes: Result<Vec<String>, core::fmt::Error> = super::ROOTS
            .iter()
            .map(|der| {
                Sha256::digest(der)
                    .iter()
                    .try_fold(String::new(), |mut hex, b| {
                        write!(hex, "{b:02x}").map(|()| hex)
                    })
            })
            .collect();
        assert_eq!(hashes, Ok(SHA256.map(String::from).to_vec()));
    }
}

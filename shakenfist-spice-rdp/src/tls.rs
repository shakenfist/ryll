//! The server's TLS identity: a certificate and key loaded from PEM
//! files, or a self-signed certificate generated at launch, with the
//! SHA-256 fingerprint a user can match against what the RDP client
//! shows.

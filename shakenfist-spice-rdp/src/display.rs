//! The RDP display handler: serves the session's primary surface to
//! the RDP client, as full-surface bitmaps after connect and after a
//! resize, and as damaged rectangles read from the surface mirror
//! otherwise.

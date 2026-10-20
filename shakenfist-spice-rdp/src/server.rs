//! The server runner: builds the IronRDP server and runs it on an OS
//! thread of its own with a current-thread runtime, because the
//! server type is not `Send`, and hands the caller a `Send` handle to
//! stop it and wait for it.
